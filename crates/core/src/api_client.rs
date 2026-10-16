//! Anthropic API client with retry and model routing.

use crate::provider::{ProviderApiStyle, ResolvedProviderProfile};
use crate::tool_executor::ToolInfo;
use crate::types::{ModelId, ToolCall, UsageSummary};
use futures::Stream;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::pin::Pin;
use thiserror::Error;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

/// Errors from API operations.
#[derive(Debug, Error)]
pub enum ApiError {
    /// HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(String),
    /// The request never completed: no connection, or no answer in time.
    ///
    /// Separate from [`Self::Http`] because whether to try again is a question
    /// about *how* it failed, and that is not something the message text can
    /// be trusted to answer — the retry decision used to be
    /// `message.contains("timeout") || message.contains("connect")`, so a
    /// rewording in the HTTP client would have quietly stopped retrying.
    #[error("transport error ({kind}): {message}")]
    Transport {
        /// What kind of failure it was.
        kind: TransportError,
        /// The underlying message, for a person.
        message: String,
    },
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
    /// Provider rejected the request because of access policy.
    #[error("access policy error: {0}")]
    AccessPolicy(String),
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
            Self::Transport { kind, message } => Self::Transport {
                kind: *kind,
                message: message.clone(),
            },
            Self::Auth(msg) => Self::Auth(msg.clone()),
            Self::Response { status, message } => Self::Response {
                status: *status,
                message: message.clone(),
            },
            Self::AccessPolicy(msg) => Self::AccessPolicy(msg.clone()),
            Self::RetryExhausted {
                attempts,
                last_error_message,
            } => Self::RetryExhausted {
                attempts: *attempts,
                last_error_message: last_error_message.clone(),
            },
            Self::InvalidModel(msg) => Self::InvalidModel(msg.clone()),
        }
    }
}

/// How a request failed at the transport level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    /// The far side did not answer in time.
    Timeout,
    /// The connection could not be established — or was dropped.
    Connect,
    /// Something else went wrong getting the request out or the answer back.
    Other,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => f.write_str("timeout"),
            Self::Connect => f.write_str("connect"),
            Self::Other => f.write_str("other"),
        }
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        // Asked of the error itself rather than read out of its message.
        let kind = if e.is_timeout() {
            TransportError::Timeout
        } else if e.is_connect() || e.is_request() {
            TransportError::Connect
        } else {
            TransportError::Other
        };
        Self::Transport {
            kind,
            message: e.to_string(),
        }
    }
}

/// Configuration for the API client.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Anthropic API key.
    pub api_key: String,
    /// Base URL for the API (default: https://api.anthropic.com).
    pub base_url: String,
    /// Set when this endpoint is a cloud provider rather than Anthropic's own
    /// API: it decides the request path, the body, and the authentication.
    pub cloud: Option<crate::providers::cloud::CloudEndpoint>,
    /// Maximum number of retry attempts for transient failures.
    pub max_retries: u32,
    /// Initial backoff duration for retries.
    pub retry_backoff: Duration,
    /// Request timeout in seconds (0 = use reqwest default).
    pub request_timeout_secs: u64,
    /// Provider interaction profile hint (e.g. "dashscope", "kimi", "anthropic").
    pub provider_profile: Option<String>,
    /// Extra request headers appended to provider requests.
    pub custom_headers: BTreeMap<String, String>,
    /// Metadata policy for outgoing requests.
    pub metadata_policy: MetadataPolicy,
    /// Optional explicit session id for request headers and metadata.
    pub session_id: Option<String>,
    /// Optional explicit device id for request metadata.
    pub device_id: Option<String>,
    /// Client name used in identity headers.
    pub client_name: String,
    /// Per-request context propagated from the engine.
    pub request_context: Option<RequestContext>,
}

/// Controls how request metadata is attached to provider requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataPolicy {
    /// Do not attach metadata.
    None,
    /// Attach session-level metadata.
    Session,
}

/// Per-request context propagated from the engine/orchestrator into provider transport.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestContext {
    /// Session identifier.
    pub session_id: Option<String>,
    /// Device identifier.
    pub device_id: Option<String>,
    /// Turn identifier within the session.
    pub turn_id: Option<String>,
    /// Optional agent identifier for forked/background flows.
    pub agent_id: Option<String>,
    /// Request correlation id.
    pub request_id: Option<String>,
    /// Provider-specific hints for shaping the transport.
    pub provider_hints: BTreeMap<String, String>,
}

impl ApiConfig {
    /// Create a config with the given API key and defaults.
    #[must_use]
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: "https://api.anthropic.com".into(),
            cloud: None,
            max_retries: 3,
            retry_backoff: Duration::from_secs(1),
            request_timeout_secs: 120,
            provider_profile: std::env::var("CODEREX_PROVIDER_PROFILE").ok(),
            custom_headers: parse_custom_headers_env(),
            metadata_policy: MetadataPolicy::Session,
            session_id: std::env::var("CODEREX_SESSION_ID").ok(),
            device_id: std::env::var("CODEREX_DEVICE_ID").ok(),
            client_name: "cli".into(),
            request_context: None,
        }
    }

    /// Create a config with custom settings.
    #[must_use]
    pub const fn with_retries(mut self, max_retries: u32, backoff: Duration) -> Self {
        self.max_retries = max_retries;
        self.retry_backoff = backoff;
        self
    }

    /// Set the request timeout.
    #[must_use]
    pub const fn with_timeout(mut self, secs: u64) -> Self {
        self.request_timeout_secs = secs;
        self
    }

    /// Set the provider profile hint.
    #[must_use]
    pub fn with_provider_profile(mut self, profile: impl Into<String>) -> Self {
        self.provider_profile = Some(profile.into());
        self
    }

    /// Add a custom header.
    #[must_use]
    pub fn with_custom_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.custom_headers.insert(key.into(), value.into());
        self
    }

    /// Set metadata policy.
    #[must_use]
    pub const fn with_metadata_policy(mut self, policy: MetadataPolicy) -> Self {
        self.metadata_policy = policy;
        self
    }

    /// Set an explicit session id.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Set an explicit device id.
    #[must_use]
    pub fn with_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = Some(device_id.into());
        self
    }

    /// Attach per-request context propagated from the engine.
    #[must_use]
    pub fn with_request_context(mut self, context: RequestContext) -> Self {
        self.request_context = Some(context);
        self
    }
}

/// Result from a non-streaming API call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse {
    /// The text content from the assistant.
    pub content: String,
    /// Tool calls extracted from structured API blocks.
    pub tool_calls: Vec<ToolCall>,
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
    /// Provider stop reason observed at end of stream
    /// ("end_turn", "max_tokens", "tool_use", "length", ...).
    StopReason(String),
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

    /// Clone the client with per-request context.
    fn with_request_context(&self, _context: RequestContext) -> Box<dyn ApiClient> {
        self.clone_box()
    }

    /// Send a non-streaming message to the API.
    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[ToolInfo],
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
        tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>>;
}

/// Anthropic API client implementation.
///
/// Supports:
/// - API key authentication via `x-api-key` header
/// - Anthropic version header
/// - Exponential backoff retry for transient failures
/// - Model routing based on `ModelId`
#[derive(Debug, Clone)]
pub struct AnthropicApiClient {
    config: ApiConfig,
    client: Client,
}

impl AnthropicApiClient {
    /// Create a new Anthropic API client with a dedicated `reqwest::Client`.
    ///
    /// Uses the timeout from `config.request_timeout_secs`. For connection
    /// pooling across multiple engines, prefer [`Self::new_with_client`].
    #[must_use]
    pub fn new(config: ApiConfig) -> Self {
        let client = if config.request_timeout_secs > 0 {
            Client::builder()
                .timeout(Duration::from_secs(config.request_timeout_secs))
                .build()
                .unwrap_or_else(|_| Client::new())
        } else {
            Client::new()
        };
        Self { config, client }
    }

    /// Create a client using an existing `reqwest::Client`.
    ///
    /// This enables connection pooling when multiple `AnthropicApiClient`
    /// instances share the same underlying `reqwest::Client`.
    #[must_use]
    pub fn new_with_client(config: ApiConfig, client: Client) -> Self {
        Self { config, client }
    }

    /// Clone this client with per-request context.
    #[must_use]
    pub fn cloned_with_request_context(&self, context: RequestContext) -> Self {
        let mut cloned = self.clone();
        cloned.config.request_context = Some(context);
        cloned
    }

    /// Resolve the model string from a ModelId.
    fn resolve_model(model: &ModelId) -> &str {
        &model.0
    }

    /// Returns true if this is the official Anthropic API.
    fn is_anthropic_api(&self) -> bool {
        let url = self
            .config
            .base_url
            .strip_suffix('/')
            .unwrap_or(&self.config.base_url);
        url == "https://api.anthropic.com"
    }

    fn user_agent() -> String {
        format!(
            "CodeRex/{} ({}; {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    }

    /// The URL a request goes to.
    ///
    /// A cloud provider puts the model in a path of its own — Bedrock names it
    /// in the path, Vertex names the project, region, and model — so the URL
    /// is built from the provider's shape rather than appended to a base.
    fn request_url(&self, model_name: &str, streaming: bool) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        let Some(cloud) = &self.config.cloud else {
            return format!("{base}/v1/messages");
        };

        let shape = crate::providers::cloud::RequestShape::for_provider(cloud.kind);
        let shape = if streaming {
            shape.streaming(cloud.kind)
        } else {
            shape
        };
        format!(
            "{base}{}",
            shape.render_path(model_name, &cloud.project, &cloud.region)
        )
    }

    /// Add what the provider needs to the request body.
    fn shape_body(&self, body: &mut serde_json::Value, model_name: &str) {
        let Some(cloud) = &self.config.cloud else {
            return;
        };
        let shape = crate::providers::cloud::RequestShape::for_provider(cloud.kind);
        shape.apply_to_body(body);

        if shape.model_in_path {
            if let Some(object) = body.as_object_mut() {
                // The model is in the path now; Bedrock rejects a body that
                // names it as well.
                object.remove("model");
            }
        }
        let _ = model_name;
    }

    /// Apply the endpoint's authentication to a request.
    ///
    /// The body is passed in because a signature covers it.
    fn apply_auth(
        &self,
        req: reqwest::RequestBuilder,
        url: &str,
        body: &[u8],
        request_id: &str,
    ) -> reqwest::RequestBuilder {
        use crate::providers::cloud::{AuthStyle, AwsCredentials, sign_v4, signing_service};

        let Some(cloud) = &self.config.cloud else {
            return if self.is_anthropic_api() {
                req.header("x-api-key", &self.config.api_key)
                    .header("anthropic-version", "2023-06-01")
            } else {
                req.header("Authorization", format!("Bearer {}", &self.config.api_key))
            };
        };

        let shape = crate::providers::cloud::RequestShape::for_provider(cloud.kind);

        // Headers the provider asks for by name; an empty value means unset.
        let mut req = req;
        for name in shape.extra_headers.keys() {
            let value = header_value(name);
            if !value.is_empty() {
                req = req.header(name.as_str(), value);
            }
        }

        match cloud.kind.auth() {
            AuthStyle::AwsSigV4 => {
                match AwsCredentials::from_env() {
                    Some(credentials) => {
                        let (host, path, query) = split_url(url);
                        let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
                        let signed = sign_v4(
                            &credentials,
                            "POST",
                            &host,
                            &path,
                            &query,
                            body,
                            &cloud.region,
                            signing_service(cloud.kind),
                            &timestamp,
                        );
                        tracing::debug!(
                            provider = cloud.kind.name(),
                            request_id = %request_id,
                            "signed request with sigv4"
                        );
                        req.header("x-amz-date", signed.amz_date)
                            .header("Authorization", signed.authorization)
                    }
                    // Bedrock and Mantle take a bearer token instead, which is
                    // what makes them usable without an AWS SDK.
                    None if cloud.kind.accepts_bearer() => {
                        req.header("Authorization", format!("Bearer {}", &self.config.api_key))
                    }
                    None => req.header("Authorization", format!("Bearer {}", &self.config.api_key)),
                }
            }
            AuthStyle::AzureApiKey => req.header("api-key", &self.config.api_key),
            AuthStyle::GoogleAccessToken | AuthStyle::Bearer => {
                req.header("Authorization", format!("Bearer {}", &self.config.api_key))
            }
            AuthStyle::AnthropicApiKey => req.header("x-api-key", &self.config.api_key),
        }
    }

    pub(crate) fn resolved_profile(&self) -> ResolvedProviderProfile {
        ResolvedProviderProfile::resolve(
            self.config.provider_profile.as_deref(),
            &self.config.base_url,
            ProviderApiStyle::AnthropicMessages,
        )
    }

    fn profile_name(&self) -> &'static str {
        self.resolved_profile().name
    }

    fn log_request_start(
        &self,
        request_id: &str,
        model_name: &str,
        streaming: bool,
        tool_count: usize,
        tool_fallback: bool,
    ) {
        tracing::info!(
            provider = "anthropic",
            profile = self.profile_name(),
            request_id = %request_id,
            model = model_name,
            base_url = %self.config.base_url,
            streaming,
            tool_count,
            tool_fallback,
            metadata_policy = ?self.config.metadata_policy,
            "provider request started"
        );
    }

    fn log_response(
        &self,
        request_id: &str,
        status: u16,
        server_request_id: Option<&str>,
        tool_fallback: bool,
    ) {
        tracing::info!(
            provider = "anthropic",
            profile = self.profile_name(),
            request_id = %request_id,
            status,
            server_request_id = server_request_id.unwrap_or(""),
            tool_fallback,
            "provider response received"
        );
    }

    fn format_error_message(
        &self,
        request_id: &str,
        server_request_id: Option<&str>,
        message: String,
        fallback_reason: Option<&str>,
    ) -> String {
        let mut diagnostics = format!("request_id={request_id}");
        if let Some(server_request_id) = server_request_id.filter(|id| !id.is_empty()) {
            diagnostics.push_str(&format!(", server_request_id={server_request_id}"));
        }
        if let Some(fallback_reason) = fallback_reason {
            diagnostics.push_str(&format!(", fallback_reason={fallback_reason}"));
        }
        format!("{message} [{diagnostics}]")
    }

    fn build_metadata_user_id(&self, request_id: &str) -> String {
        let session_id = self
            .config
            .request_context
            .as_ref()
            .and_then(|ctx| ctx.session_id.clone())
            .or_else(|| self.config.session_id.clone())
            .or_else(|| std::env::var("CODEREX_SESSION_ID").ok())
            .unwrap_or_else(|| "unknown".into());
        let device_id = self
            .config
            .request_context
            .as_ref()
            .and_then(|ctx| ctx.device_id.clone())
            .or_else(|| self.config.device_id.clone())
            .or_else(|| std::env::var("CODEREX_DEVICE_ID").ok())
            .unwrap_or_else(|| "unknown".into());
        serde_json::json!({
            "device_id": device_id,
            "session_id": session_id,
            "turn_id": self.config.request_context.as_ref().and_then(|ctx| ctx.turn_id.clone()).unwrap_or_else(|| "unknown".into()),
            "agent_id": self.config.request_context.as_ref().and_then(|ctx| ctx.agent_id.clone()).unwrap_or_else(|| "main".into()),
            "client": self.config.client_name,
            "request_id": request_id,
        })
        .to_string()
    }

    fn apply_standard_headers(
        &self,
        mut req: reqwest::RequestBuilder,
        request_id: &str,
    ) -> reqwest::RequestBuilder {
        let user_agent = Self::user_agent();
        req = req
            .header("User-Agent", user_agent.clone())
            .header("X-CodeRex-Client", &self.config.client_name)
            .header("X-CodeRex-Request-Id", request_id);

        if let Some(session_id) = self
            .config
            .request_context
            .as_ref()
            .and_then(|ctx| ctx.session_id.clone())
            .or_else(|| self.config.session_id.clone())
            .or_else(|| std::env::var("CODEREX_SESSION_ID").ok())
        {
            if !session_id.trim().is_empty() {
                req = req.header("X-CodeRex-Session-Id", session_id);
            }
        }

        if let Some(agent_id) = self
            .config
            .request_context
            .as_ref()
            .and_then(|ctx| ctx.agent_id.clone())
        {
            if !agent_id.trim().is_empty() {
                req = req.header("X-CodeRex-Agent-Id", agent_id);
            }
        }

        if self.profile_name() == "dashscope" {
            req = req
                .header("X-DashScope-UserAgent", user_agent)
                .header("X-DashScope-CacheControl", "enable")
                .header(
                    "X-DashScope-AuthType",
                    std::env::var("CODEREX_AUTHTYPE").unwrap_or_else(|_| "api_key".into()),
                );
        }

        for (k, v) in &self.config.custom_headers {
            req = req.header(k, v);
        }

        req
    }

    fn attach_metadata(&self, body: &mut serde_json::Value, request_id: &str) {
        if self.config.metadata_policy == MetadataPolicy::None {
            return;
        }
        if let Some(obj) = body.as_object_mut() {
            if obj.contains_key("metadata") {
                return;
            }
            let session_context = serde_json::json!({
                "session_id": self.config.request_context.as_ref().and_then(|ctx| ctx.session_id.clone()).or_else(|| self.config.session_id.clone()).unwrap_or_else(|| "unknown".into()),
                "turn_id": self.config.request_context.as_ref().and_then(|ctx| ctx.turn_id.clone()).unwrap_or_else(|| "unknown".into()),
                "agent_id": self.config.request_context.as_ref().and_then(|ctx| ctx.agent_id.clone()).unwrap_or_else(|| "main".into()),
            });
            obj.insert(
                "metadata".to_string(),
                serde_json::json!({
                    "user_id": self.build_metadata_user_id(request_id),
                    "session_context": session_context,
                }),
            );
        }
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
                tracing::debug!(
                    "API retry {attempt}/{}, backing off {backoff:?}",
                    self.config.max_retries
                );
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

    fn with_request_context(&self, context: RequestContext) -> Box<dyn ApiClient> {
        Box::new(self.cloned_with_request_context(context))
    }

    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        if self.config.api_key.is_empty() {
            return Err(ApiError::Auth(
                "ANTHROPIC_API_KEY environment variable is not set".into(),
            ));
        }

        let model_name = Self::resolve_model(model);
        let url = self.request_url(model_name, false);
        let request_id = Uuid::new_v4().to_string();

        let mut body = serde_json::json!({
            "model": model_name,
            "system": system,
            "messages": messages,
            "max_tokens": max_tokens,
        });
        self.attach_metadata(&mut body, &request_id);
        self.shape_body(&mut body, model_name);

        let capabilities = self.resolved_profile().capabilities;
        let tool_fallback = !tools.is_empty() && !capabilities.tool_use;
        self.log_request_start(
            &request_id,
            &model_name,
            false,
            if capabilities.tool_use {
                tools.len()
            } else {
                0
            },
            tool_fallback,
        );

        if !tools.is_empty() && capabilities.tool_use {
            let tools_json: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();
            body.as_object_mut()
                .unwrap()
                .insert("tools".to_string(), serde_json::Value::Array(tools_json));
        }

        // The bytes are serialized here rather than by `json()`, because a
        // signed request has to hash exactly what it sends.
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| ApiError::Http(format!("could not serialize the request: {e}")))?;

        let response = self
            .with_retry(|| async {
                let mut req = self
                    .client
                    .post(&url)
                    .header("content-type", "application/json")
                    // Anthropic-compatible gateways (Qwen/Kimi/etc.) may expect this
                    // version header even when the host is not api.anthropic.com.
                    .header("anthropic-version", "2023-06-01");
                req = self.apply_standard_headers(req, &request_id);
                req = self.apply_auth(req, &url, &body_bytes, &request_id);

                let resp = req
                    .body(body_bytes.clone())
                    .send()
                    .await
                    .map_err(ApiError::from)?;

                let status = resp.status().as_u16();
                let server_request_id = resp
                    .headers()
                    .get("x-request-id")
                    .or_else(|| resp.headers().get("request-id"))
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                if status >= 400 {
                    let err_text = self.format_error_message(
                        &request_id,
                        server_request_id.as_deref(),
                        resp.text().await.unwrap_or_default(),
                        None,
                    );
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

                let content_blocks = json["content"].as_array();
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();
                if let Some(blocks) = content_blocks {
                    for block in blocks {
                        match block.get("type").and_then(|v| v.as_str()) {
                            Some("text") => {
                                if let Some(t) = block["text"].as_str() {
                                    text_parts.push(t);
                                }
                            }
                            Some("tool_use") => {
                                if let (Some(id), Some(name)) =
                                    (block["id"].as_str(), block["name"].as_str())
                                {
                                    tool_calls.push(ToolCall {
                                        id: id.to_string(),
                                        name: name.to_string(),
                                        input: block["input"].clone(),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
                let content = text_parts.join("");

                let stop_reason = json["stop_reason"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();

                #[allow(clippy::cast_possible_truncation)]
                let usage = UsageSummary {
                    input_tokens: json["usage"]["input_tokens"].as_u64().unwrap_or(0) as usize,
                    output_tokens: json["usage"]["output_tokens"].as_u64().unwrap_or(0) as usize,
                    cache_read_tokens: json["usage"]["cache_read_input_tokens"]
                        .as_u64()
                        .unwrap_or(0) as usize,
                    cache_creation_tokens: json["usage"]["cache_creation_input_tokens"]
                        .as_u64()
                        .unwrap_or(0) as usize,
                };

                tracing::info!(
                    provider = "anthropic",
                    profile = self.profile_name(),
                    request_id = %request_id,
                    status,
                    server_request_id = server_request_id.as_deref().unwrap_or(""),
                    tool_fallback,
                    "provider response received"
                );

                Ok(ApiResponse {
                    content,
                    tool_calls,
                    usage,
                    stop_reason,
                })
            })
            .await?;

        self.log_response(&request_id, 200, None, tool_fallback);

        Ok(response)
    }

    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        if self.config.api_key.is_empty() {
            return Box::pin(futures::stream::once(async {
                StreamEvent::Error(ApiError::Auth(
                    "ANTHROPIC_API_KEY environment variable is not set".into(),
                ))
            }));
        }

        let model_name = Self::resolve_model(model).to_string();
        let url = self.request_url(&model_name, true);
        let request_id = Uuid::new_v4().to_string();
        let mut body = serde_json::json!({
            "model": model_name.clone(),
            "system": system,
            "messages": messages,
            "max_tokens": max_tokens,
            "stream": true,
        });
        self.attach_metadata(&mut body, &request_id);
        self.shape_body(&mut body, &model_name);

        let capabilities = self.resolved_profile().capabilities;
        let tool_fallback = !tools.is_empty() && !capabilities.tool_use;
        self.log_request_start(
            &request_id,
            &model_name,
            true,
            if capabilities.tool_use {
                tools.len()
            } else {
                0
            },
            tool_fallback,
        );

        if !tools.is_empty() && capabilities.tool_use {
            let tools_json: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();
            body.as_object_mut()
                .unwrap()
                .insert("tools".to_string(), serde_json::Value::Array(tools_json));
        }

        let client = self.client.clone();
        let config = self.config.clone();

        Box::pin(
            futures::stream::once(async move {
                let temp_client = AnthropicApiClient {
                    config,
                    client: client.clone(),
                };
                tracing::debug!(
                    provider = "anthropic",
                    profile = temp_client.profile_name(),
                    request_id = %request_id,
                    body = %serde_json::to_string(&body).unwrap_or_default(),
                    "starting streaming provider request"
                );
                let mut req = client
                    .post(&url)
                    .header("content-type", "application/json")
                    // Anthropic-compatible gateways (Qwen/Kimi/etc.) may expect this
                    // version header even when the host is not api.anthropic.com.
                    .header("anthropic-version", "2023-06-01");
                req = temp_client.apply_standard_headers(req, &request_id);

                let body_bytes = match serde_json::to_vec(&body) {
                    Ok(bytes) => bytes,
                    // This closure returns a stream, so a failure here is an
                    // error event rather than a `Result`.
                    Err(error) => {
                        let message = format!("could not serialize the request: {error}");
                        return futures::stream::once(async move {
                            StreamEvent::Error(ApiError::Http(message))
                        })
                        .chain(futures::stream::once(async move { StreamEvent::StreamEnd }))
                        .boxed();
                    }
                };
                req = temp_client.apply_auth(req, &url, &body_bytes, &request_id);

                let response = req.body(body_bytes).send().await;

                match response {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let server_request_id = resp
                            .headers()
                            .get("x-request-id")
                            .or_else(|| resp.headers().get("request-id"))
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        if status >= 400 {
                            let err_text = temp_client.format_error_message(
                                &request_id,
                                server_request_id.as_deref(),
                                resp.text().await.unwrap_or_default(),
                                None,
                            );
                            if err_text.to_ascii_lowercase().contains("coding agent")
                                || err_text.to_ascii_lowercase().contains("access policy")
                            {
                                return futures::stream::once(async move {
                                    StreamEvent::Error(ApiError::AccessPolicy(err_text))
                                })
                                .chain(futures::stream::once(async move { StreamEvent::StreamEnd }))
                                .boxed();
                            }
                            return futures::stream::once(async move {
                                StreamEvent::Error(ApiError::Response {
                                    status,
                                    message: err_text,
                                })
                            })
                            .chain(futures::stream::once(async move { StreamEvent::StreamEnd }))
                            .boxed();
                        }

                        tracing::info!(
                            provider = "anthropic",
                            profile = temp_client.profile_name(),
                            request_id = %request_id,
                            status,
                            server_request_id = server_request_id.as_deref().unwrap_or(""),
                            tool_fallback,
                            "streaming provider response started"
                        );

                        parse_sse_stream(resp.bytes_stream()).boxed()
                    }
                    Err(e) => {
                        futures::stream::once(async move { StreamEvent::Error(ApiError::from(e)) })
                            .chain(futures::stream::once(async move { StreamEvent::StreamEnd }))
                            .boxed()
                    }
                }
            })
            .flatten(),
        )
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
    /// Input-JSON deltas that arrived before their `content_block_start`.
    /// Buffered and flushed once the mapping is known.
    deferred_deltas: Vec<(usize, String)>,
}

/// The value of a provider header, read from the environment.
///
/// The one that matters is Bedrock's service tier, which is how a deployment
/// asks for a capacity tier; unset means the provider's default.
fn header_value(name: &str) -> String {
    match name {
        "X-Amzn-Bedrock-Service-Tier" => {
            std::env::var("ANTHROPIC_BEDROCK_SERVICE_TIER").unwrap_or_default()
        }
        other => {
            let key = format!("ANTHROPIC_{}", other.replace('-', "_").to_ascii_uppercase());
            std::env::var(key).unwrap_or_default()
        }
    }
}

/// Split a URL into the host, path, and query a signature needs.
fn split_url(url: &str) -> (String, String, String) {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);

    let (authority, rest) = match without_scheme.find('/') {
        Some(index) => (&without_scheme[..index], &without_scheme[index..]),
        None => (without_scheme, ""),
    };

    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (
            if rest.is_empty() {
                "/".to_string()
            } else {
                rest.to_string()
            },
            String::new(),
        ),
    };

    let host = authority.rsplit('@').next().unwrap_or(authority);
    (host.to_string(), path, query)
}

fn parse_sse_stream(
    stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl futures::Stream<Item = StreamEvent> + Send + 'static {
    use crate::stream_utils::Utf8ChunkDecoder;
    use futures::StreamExt;

    let pinned = Box::pin(stream);

    futures::stream::unfold(
        (
            pinned,
            String::new(),
            SseParserState::default(),
            Utf8ChunkDecoder::new(),
            Vec::<StreamEvent>::new(),
        ),
        |(mut inner, mut buffer, mut state, mut decoder, mut outbox)| async move {
            loop {
                if let Some(event) = (!outbox.is_empty()).then(|| outbox.remove(0)) {
                    return Some((event, (inner, buffer, state, decoder, outbox)));
                }

                if let Some(pos) = crate::stream_utils::find_sse_message_boundary(&buffer) {
                    let message = buffer[..pos].to_string();
                    buffer.drain(..=pos);

                    let parsed = parse_sse_message(&message, &mut state);
                    let mut it = parsed.into_iter();
                    if let Some(first) = it.next() {
                        outbox = it.collect();
                        return Some((first, (inner, buffer, state, decoder, outbox)));
                    }
                    continue;
                }

                match inner.next().await {
                    Some(Ok(chunk)) => {
                        // Incremental decode: incomplete trailing multibyte
                        // sequences are held back instead of becoming U+FFFD.
                        buffer.push_str(&decoder.push(&chunk));
                    }
                    Some(Err(e)) => {
                        tracing::warn!("SSE stream network error: {e}");
                        buffer.push_str(&decoder.finish());
                        return Some((
                            StreamEvent::Error(ApiError::from(e)),
                            (inner, buffer, state, decoder, outbox),
                        ));
                    }
                    None => {
                        buffer.push_str(&decoder.finish());
                        return None;
                    }
                }
            }
        },
    )
    .chain(futures::stream::once(async move { StreamEvent::StreamEnd }))
}

/// Parse a single SSE message block into zero or more `StreamEvent`s.
///
/// `message_delta` may carry both usage and stop reason; callers consume the
/// returned events in order.
fn parse_sse_message(message: &str, state: &mut SseParserState) -> Vec<StreamEvent> {
    // Filter out SSE comment lines (starting with ':') and blank lines,
    // then extract the first `data:` line.
    let Some(data_line) = message
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with(':')
        })
        .find(|line| line.starts_with("data:"))
    else {
        return Vec::new();
    };
    let Some(json_str) = data_line.strip_prefix("data:").map(|s| s.trim_start()) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return Vec::new();
    };
    let Some(event_type) = value.get("type").and_then(|v| v.as_str()) else {
        return Vec::new();
    };

    match event_type {
        "content_block_start" => {
            let Some(index) = value
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .map(|i| i as usize)
            else {
                return Vec::new();
            };
            let Some(block) = value.get("content_block") else {
                return Vec::new();
            };
            let Some("tool_use") = block.get("type").and_then(serde_json::Value::as_str) else {
                return Vec::new();
            };
            let Some(id) = block.get("id").and_then(serde_json::Value::as_str) else {
                return Vec::new();
            };
            let Some(name) = block.get("name").and_then(serde_json::Value::as_str) else {
                return Vec::new();
            };
            let id = id.to_string();
            state.index_to_id.insert(index, id.clone());
            let mut out = vec![StreamEvent::ToolCallBlockStart {
                id: id.clone(),
                name: name.to_string(),
            }];
            // Flush any input deltas that arrived before the mapping existed.
            let deferred = std::mem::take(&mut state.deferred_deltas);
            for (didx, delta) in deferred {
                if didx == index {
                    out.push(StreamEvent::ToolCallInputDelta {
                        id: id.clone(),
                        delta,
                    });
                } else {
                    state.deferred_deltas.push((didx, delta));
                }
            }
            out
        }
        "content_block_delta" => {
            let Some(index) = value
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .map(|i| i as usize)
            else {
                return Vec::new();
            };
            let Some(delta) = value.get("delta") else {
                return Vec::new();
            };
            let Some(delta_type) = delta.get("type").and_then(serde_json::Value::as_str) else {
                return Vec::new();
            };
            match delta_type {
                "text_delta" => {
                    let Some(text) = delta.get("text").and_then(serde_json::Value::as_str) else {
                        return Vec::new();
                    };
                    if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![StreamEvent::TextDelta(text.to_string())]
                    }
                }
                "input_json_delta" => {
                    let Some(partial_json) = delta
                        .get("partial_json")
                        .and_then(serde_json::Value::as_str)
                    else {
                        return Vec::new();
                    };
                    let partial_json = partial_json.to_string();
                    match state.index_to_id.get(&index) {
                        Some(id) => vec![StreamEvent::ToolCallInputDelta {
                            id: id.clone(),
                            delta: partial_json,
                        }],
                        None => {
                            // Mapping not yet established: buffer the delta
                            // until `content_block_start` registers the index.
                            state.deferred_deltas.push((index, partial_json));
                            Vec::new()
                        }
                    }
                }
                _ => Vec::new(),
            }
        }
        "content_block_stop" => {
            let Some(index) = value
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .map(|i| i as usize)
            else {
                return Vec::new();
            };
            let Some(id) = state.index_to_id.remove(&index) else {
                return Vec::new();
            };
            vec![StreamEvent::ToolCallBlockEnd {
                id,
                input: serde_json::Value::Null,
            }]
        }
        "message_delta" => {
            #[allow(clippy::cast_possible_truncation)]
            let usage = value.get("usage").map(|u| UsageSummary {
                input_tokens: u
                    .get("input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize,
                output_tokens: u
                    .get("output_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize,
                cache_read_tokens: u
                    .get("cache_read_input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize,
                cache_creation_tokens: u
                    .get("cache_creation_input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize,
            });
            let stop_reason = value
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let mut events = Vec::with_capacity(2);
            if let Some(u) = usage {
                events.push(StreamEvent::UsageSummary(u));
            }
            if let Some(r) = stop_reason {
                events.push(StreamEvent::StopReason(r));
            }
            events
        }
        _ => Vec::new(),
    }
}
/// Classify an error as retryable (transient): timeouts, connection failures,
/// server errors and rate limits. Auth/policy and other 4xx errors are not.
#[must_use]
pub fn is_retryable(error: &ApiError) -> bool {
    match error {
        ApiError::Transport { kind, .. } => {
            matches!(kind, TransportError::Timeout | TransportError::Connect)
        }
        // A message we built ourselves, not a transport failure.
        ApiError::Http(_) => false,
        ApiError::Response { status, .. } => *status >= 500 || *status == 429,
        ApiError::AccessPolicy(_) => false,
        _ => false,
    }
}

fn parse_custom_headers(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if !key.is_empty() {
                out.insert(key.to_string(), val.to_string());
            }
        }
    }
    out
}

fn parse_custom_headers_env() -> BTreeMap<String, String> {
    match std::env::var("CODEREX_CUSTOM_HEADERS") {
        Ok(v) => parse_custom_headers(&v),
        Err(_) => BTreeMap::new(),
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
            .create_message("system", &[], &ModelId::sonnet(), 1024, &[])
            .await;

        assert!(matches!(result, Err(ApiError::Auth(_))));
    }

    fn cloud_client(
        kind: crate::providers::cloud::ProviderKind,
        region: &str,
    ) -> AnthropicApiClient {
        let mut config = ApiConfig::new("test-key".into());
        config.base_url = crate::providers::default_base_url(kind, region);
        config.cloud = Some(crate::providers::cloud::CloudEndpoint::new(kind, region));
        AnthropicApiClient::new(config)
    }

    #[test]
    fn a_plain_endpoint_still_uses_the_anthropic_path() {
        let client = AnthropicApiClient::new(ApiConfig::new("k".into()));
        assert_eq!(
            client.request_url("claude-sonnet-4-6", false),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn bedrock_names_the_model_in_the_path_and_stamps_the_body() {
        let client = cloud_client(crate::providers::cloud::ProviderKind::Bedrock, "us-east-1");
        assert_eq!(
            client.request_url("claude-sonnet-4-6", false),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/claude-sonnet-4-6/invoke"
        );
        assert_eq!(
            client.request_url("claude-sonnet-4-6", true),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/claude-sonnet-4-6/\
             invoke-with-response-stream"
        );

        let mut body = serde_json::json!({"model": "claude-sonnet-4-6", "max_tokens": 10});
        client.shape_body(&mut body, "claude-sonnet-4-6");
        assert_eq!(body["anthropic_version"], "bedrock-2023-05-31");
        // The model is in the path, so it leaves the body.
        assert!(body.get("model").is_none(), "{body}");
    }

    #[test]
    fn vertex_names_the_project_and_region_in_the_path() {
        let mut client = cloud_client(crate::providers::cloud::ProviderKind::Vertex, "us-central1");
        client.config.cloud = Some(
            crate::providers::cloud::CloudEndpoint::new(
                crate::providers::cloud::ProviderKind::Vertex,
                "us-central1",
            )
            .with_project("my-project"),
        );

        assert_eq!(
            client.request_url("claude-sonnet-4-6", false),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/\
             us-central1/publishers/anthropic/models/claude-sonnet-4-6:rawPredict"
        );

        let mut body = serde_json::json!({"model": "claude-sonnet-4-6"});
        client.shape_body(&mut body, "claude-sonnet-4-6");
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
        // Vertex keeps the model in the body as well as the path.
        assert_eq!(body["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn a_url_is_split_the_way_a_signature_needs() {
        assert_eq!(
            split_url("https://bedrock-runtime.us-east-1.amazonaws.com/model/m/invoke"),
            (
                "bedrock-runtime.us-east-1.amazonaws.com".to_string(),
                "/model/m/invoke".to_string(),
                String::new()
            )
        );
        assert_eq!(
            split_url("https://example.com/path?a=1&b=2"),
            (
                "example.com".to_string(),
                "/path".to_string(),
                "a=1&b=2".to_string()
            )
        );
        assert_eq!(
            split_url("https://example.com"),
            ("example.com".to_string(), "/".to_string(), String::new())
        );
    }

    #[tokio::test]
    async fn config_defaults_are_correct() {
        let config = ApiConfig::new("test_key".into());
        assert_eq!(config.base_url, "https://api.anthropic.com");
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.retry_backoff, Duration::from_secs(1));
        assert_eq!(config.request_timeout_secs, 120);
        assert_eq!(config.metadata_policy, MetadataPolicy::Session);
        assert_eq!(config.client_name, "cli");
    }

    #[tokio::test]
    async fn config_custom_timeout() {
        let config = ApiConfig::new("test_key".into()).with_timeout(60);
        assert_eq!(config.request_timeout_secs, 60);
    }

    /// The retry decision comes from how the request failed, not from what the
    /// failure says. It used to be `message.contains("timeout") ||
    /// message.contains("connect")` — so a rewording in the HTTP client, or a
    /// message that happened to contain "connect" for another reason, changed
    /// whether the client retried.
    #[test]
    fn is_retryable_reads_the_kind_not_the_message() {
        let transport = |kind, message: &str| ApiError::Transport {
            kind,
            message: message.to_string(),
        };

        assert!(is_retryable(&transport(
            TransportError::Timeout,
            "something entirely different"
        )));
        assert!(is_retryable(&transport(TransportError::Connect, "")));

        // A message that mentions the words is not a timeout.
        assert!(!is_retryable(&transport(
            TransportError::Other,
            "timed out while connecting"
        )));
        // And one that is a failure of ours is not retryable either.
        assert!(!is_retryable(&ApiError::Http(
            "timeout while connecting".to_string()
        )));
    }

    #[test]
    fn a_request_error_carries_its_kind() {
        // The conversion is what fills the kind in; a `reqwest::Error` cannot
        // be built here, so this pins the mapping the classification relies on.
        for (kind, retryable) in [
            (TransportError::Timeout, true),
            (TransportError::Connect, true),
            (TransportError::Other, false),
        ] {
            let error = ApiError::Transport {
                kind,
                message: String::new(),
            };
            assert_eq!(is_retryable(&error), retryable, "{kind}");
        }
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
        assert_eq!(
            AnthropicApiClient::resolve_model(&ModelId::sonnet()),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            AnthropicApiClient::resolve_model(&ModelId::opus()),
            "claude-opus-4-6"
        );
        assert_eq!(
            AnthropicApiClient::resolve_model(&ModelId::haiku()),
            "claude-haiku-4-5-20251001"
        );
    }

    #[test]
    fn parse_custom_headers_from_env() {
        let headers = parse_custom_headers("X-Test-Header: foo\nX-Trace: bar\ninvalid-line");
        assert_eq!(headers.get("X-Test-Header"), Some(&"foo".to_string()));
        assert_eq!(headers.get("X-Trace"), Some(&"bar".to_string()));
    }

    #[test]
    fn apply_standard_headers_sets_agent_identity_headers() {
        let mut config = ApiConfig::new("key".into());
        config.provider_profile = Some("dashscope".into());
        config.custom_headers.insert("X-Custom".into(), "1".into());
        let client = AnthropicApiClient::new(config);
        let req = client.client.post("https://example.com");
        let req = client.apply_standard_headers(req, "req-123");
        let built = req.build().expect("request should build");
        let headers = built.headers();
        assert!(headers.get("User-Agent").is_some());
        assert_eq!(headers.get("X-CodeRex-Client").unwrap(), "cli");
        assert_eq!(headers.get("X-CodeRex-Request-Id").unwrap(), "req-123");
        assert_eq!(headers.get("X-Custom").unwrap(), "1");
        assert_eq!(headers.get("X-DashScope-CacheControl").unwrap(), "enable");
    }

    #[test]
    fn attach_metadata_respects_policy() {
        let mut body = serde_json::json!({
            "model": "x",
            "messages": []
        });
        let client = AnthropicApiClient::new(ApiConfig::new("key".into()));
        client.attach_metadata(&mut body, "req-1");
        assert!(body.get("metadata").is_some());

        let mut body_no_meta = serde_json::json!({
            "model": "x",
            "messages": []
        });
        let cfg = ApiConfig::new("key".into()).with_metadata_policy(MetadataPolicy::None);
        let client = AnthropicApiClient::new(cfg);
        client.attach_metadata(&mut body_no_meta, "req-2");
        assert!(body_no_meta.get("metadata").is_none());
    }

    // ─── SSE parser tests ───────────────────────────────────────────────

    #[test]
    fn parse_sse_message_text_delta() {
        let msg = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n";
        let mut state = SseParserState::default();
        let events = parse_sse_message(msg, &mut state);
        assert!(matches!(events.as_slice(), [StreamEvent::TextDelta(t)] if t == "Hello"));
    }

    #[test]
    fn parse_sse_message_tool_use_start() {
        let msg = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01ABC\",\"name\":\"read\",\"input\":{}}}\n\n";
        let mut state = SseParserState::default();
        let events = parse_sse_message(msg, &mut state);
        assert!(
            matches!(events.as_slice(), [StreamEvent::ToolCallBlockStart { id, name }] if id == "toolu_01ABC" && name == "read")
        );
        assert_eq!(state.index_to_id.get(&1), Some(&"toolu_01ABC".to_string()));
    }

    #[test]
    fn parse_sse_message_input_json_delta() {
        let mut state = SseParserState::default();
        state.index_to_id.insert(1, "toolu_01ABC".to_string());

        let msg = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file\\\":\\\"test.txt\\\"}\"}}\n\n";
        let events = parse_sse_message(msg, &mut state);
        assert!(
            matches!(events.as_slice(), [StreamEvent::ToolCallInputDelta { id, delta }] if id == "toolu_01ABC" && delta == "{\"file\":\"test.txt\"}")
        );
    }

    #[test]
    fn parse_sse_message_content_block_stop() {
        let mut state = SseParserState::default();
        state.index_to_id.insert(1, "toolu_01ABC".to_string());

        let msg =
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n";
        let events = parse_sse_message(msg, &mut state);
        assert!(
            matches!(events.as_slice(), [StreamEvent::ToolCallBlockEnd { id, input }] if id == "toolu_01ABC" && input.is_null())
        );
        assert!(!state.index_to_id.contains_key(&1));
    }

    #[test]
    fn parse_sse_message_usage_summary() {
        let msg = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":100,\"output_tokens\":50}}\n\n";
        let mut state = SseParserState::default();
        let events = parse_sse_message(msg, &mut state);
        assert!(
            matches!(events.as_slice(), [StreamEvent::UsageSummary(u), StreamEvent::StopReason(r)] if u.input_tokens == 100 && u.output_tokens == 50 && r == "end_turn")
        );
    }

    #[test]
    fn parse_sse_message_ignores_unknown_types() {
        let msg = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
        let mut state = SseParserState::default();
        let events = parse_sse_message(msg, &mut state);
        assert!(events.is_empty());
    }

    #[test]
    fn parse_sse_message_ignores_text_block_start() {
        let msg = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
        let mut state = SseParserState::default();
        let events = parse_sse_message(msg, &mut state);
        assert!(events.is_empty());
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

        assert_eq!(events.len(), 7); // text + tool_start + tool_delta + tool_end + usage + stop_reason + stream_end
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "I'll read the file."));
        assert!(
            matches!(&events[1], StreamEvent::ToolCallBlockStart { id, name } if id == "toolu_01XYZ" && name == "read")
        );
        assert!(
            matches!(&events[2], StreamEvent::ToolCallInputDelta { id, delta } if id == "toolu_01XYZ" && delta == "{\"file\":\"hello.txt\"}")
        );
        assert!(
            matches!(&events[3], StreamEvent::ToolCallBlockEnd { id, .. } if id == "toolu_01XYZ")
        );
        assert!(
            matches!(&events[4], StreamEvent::UsageSummary(u) if u.input_tokens == 10 && u.output_tokens == 20)
        );
        assert!(matches!(&events[5], StreamEvent::StopReason(r) if r == "end_turn"));
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
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = sse
            .bytes()
            .map(|b| Ok(bytes::Bytes::from(vec![b])))
            .collect();

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

        let tool_starts = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolCallBlockStart { .. }))
            .count();
        let tool_deltas = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolCallInputDelta { .. }))
            .count();
        let tool_ends = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolCallBlockEnd { .. }))
            .count();

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
