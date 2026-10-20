//! Anthropic API client with retry and model routing.

use crate::provider::{ProviderApiStyle, ReasoningEffortSupport, ResolvedProviderProfile};
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
        /// What kind of failure it was, coarsely.
        kind: TransportError,
        /// The triage class, from the facts of the failure (see
        /// [`classify_transport_facts`]). Finer than `kind` because the retry
        /// decision needs to tell a cut-off stream from a certificate the
        /// server will reject again.
        class: ErrorClass,
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
            Self::Transport {
                kind,
                class,
                message,
            } => Self::Transport {
                kind: *kind,
                class: *class,
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
///
/// Coarse on purpose. Other subsystems read this bucket to answer one
/// question — did it time out, did it fail to connect, or something else —
/// and the finer answer lives on [`ApiError::Transport::class`].
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

// ─── Provider error triage ──────────────────────────────────────────────────

/// The class a provider failure falls into.
///
/// This is the table the retry decision reads. Every class is settled by a
/// *fact* about the failure — a predicate the HTTP client offers about itself
/// (`is_timeout`, `is_body`, `is_connect`, …), an HTTP status, or the
/// [`std::io::ErrorKind`] at the bottom of the source chain — never by
/// matching the error's wording. That property is the point: the previous
/// retry rule read the message, so a rewording in the HTTP client silently
/// changed whether a request was retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// TLS handshake or certificate failure. Sending the same request again
    /// fails the same way.
    Ssl,
    /// The host name could not be resolved. Worth another try — resolution
    /// failures are as often a blip as a bad name.
    Dns,
    /// The connection was refused, reset, or dropped.
    Connection,
    /// The far side did not answer in time.
    Timeout,
    /// The response body was cut off part-way through the stream.
    Stream,
    /// A response arrived but could not be decoded. The bytes are what they
    /// are, so re-asking does not help.
    Decode,
    /// The provider asked us to slow down (HTTP 429).
    RateLimit,
    /// The provider failed on its side (HTTP 5xx).
    Server,
    /// Credentials were missing or rejected (HTTP 401/403).
    Auth,
    /// The request was refused by the provider's access policy.
    AccessPolicy,
    /// The request itself was wrong (bad model, bad shape, other 4xx).
    InvalidRequest,
    /// Not classified.
    Unknown,
}

impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Ssl => "ssl",
            Self::Dns => "dns",
            Self::Connection => "connection",
            Self::Timeout => "timeout",
            Self::Stream => "stream",
            Self::Decode => "decode",
            Self::RateLimit => "rate limit",
            Self::Server => "server",
            Self::Auth => "auth",
            Self::AccessPolicy => "access policy",
            Self::InvalidRequest => "invalid request",
            Self::Unknown => "unknown",
        };
        f.write_str(name)
    }
}

impl ErrorClass {
    /// The coarse [`TransportError`] bucket this class belongs to.
    ///
    /// Only meaningful for the classes a transport failure produces; the
    /// others fall to `Other`.
    #[must_use]
    pub const fn transport_kind(self) -> TransportError {
        match self {
            Self::Timeout => TransportError::Timeout,
            // Both are connection-stage failures, and both are worth another
            // try, which is what the coarse bucket says.
            Self::Dns | Self::Connection => TransportError::Connect,
            // A certificate the server will reject again, a body that will
            // not decode, an unclassified failure: none of them are "connect".
            Self::Ssl
            | Self::Stream
            | Self::Decode
            | Self::Unknown
            | Self::RateLimit
            | Self::Server
            | Self::Auth
            | Self::AccessPolicy
            | Self::InvalidRequest => TransportError::Other,
        }
    }
}

/// What to do about a classified failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAction {
    /// Try again after a backoff.
    Retry,
    /// Try again, but back off further: the provider has told us to slow down,
    /// and retrying a rate limit at the old pace is how a soft limit becomes
    /// a hard one.
    RetryAfterRateLimit,
    /// Do not try again. The same request will fail the same way.
    Fail,
}

/// A classified failure and the decision it implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorTriage {
    /// What kind of failure it was.
    pub class: ErrorClass,
    /// Whether and how to try again.
    pub action: RetryAction,
}

impl ErrorTriage {
    /// Whether this failure should be retried.
    #[must_use]
    pub const fn is_retry(&self) -> bool {
        !matches!(self.action, RetryAction::Fail)
    }
}

/// The structured facts a transport failure is classified from.
///
/// `reqwest::Error` answers these about itself; nothing here is read out of
/// an error message. `io_kind` is the one fact the HTTP client does not
/// expose directly — the [`std::io::Error`] underneath a connect failure
/// still carries a kind, and that is how a TLS/certificate failure is told
/// from a name that would not resolve without matching on text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransportFacts {
    /// The request timed out.
    pub timeout: bool,
    /// The response body failed mid-stream.
    pub body: bool,
    /// The response body could not be decoded.
    pub decode: bool,
    /// The connection could not be established.
    pub connect: bool,
    /// The request itself failed.
    pub request: bool,
    /// The kind of the `io::Error` at the bottom of the source chain, if any.
    pub io_kind: Option<std::io::ErrorKind>,
}

/// Classify a transport failure from its facts.
///
/// The order matters: a timeout is a timeout whatever else is also true, and
/// a body that failed after a response arrived is a stream failure rather
/// than a connect failure.
#[must_use]
pub const fn classify_transport_facts(facts: TransportFacts) -> ErrorClass {
    if facts.timeout {
        return ErrorClass::Timeout;
    }
    if facts.decode {
        return ErrorClass::Decode;
    }
    if facts.body {
        return ErrorClass::Stream;
    }
    if facts.connect || facts.request {
        return match facts.io_kind {
            // A handshake or certificate failure surfaces as a data-validity
            // error, not as a plain connection refusal.
            Some(std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof) => {
                ErrorClass::Ssl
            }
            // Name resolution failed.
            Some(std::io::ErrorKind::NotFound) => ErrorClass::Dns,
            _ => ErrorClass::Connection,
        };
    }
    ErrorClass::Unknown
}

/// The kind of the `std::io::Error` at the bottom of an error's source chain.
fn io_error_kind(error: &(dyn std::error::Error + 'static)) -> Option<std::io::ErrorKind> {
    let mut source = error.source();
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return Some(io.kind());
        }
        source = err.source();
    }
    None
}

/// Classify an [`ApiError`].
#[must_use]
pub const fn classify_error(error: &ApiError) -> ErrorClass {
    match error {
        // The class was settled when the error was built, from the facts the
        // HTTP client reported about itself.
        ApiError::Transport { class, .. } => *class,
        ApiError::Auth(_) => ErrorClass::Auth,
        ApiError::AccessPolicy(_) => ErrorClass::AccessPolicy,
        ApiError::Response { status, .. } => match status {
            401 | 403 => ErrorClass::Auth,
            429 => ErrorClass::RateLimit,
            500..=599 => ErrorClass::Server,
            _ => ErrorClass::InvalidRequest,
        },
        ApiError::InvalidModel(_) => ErrorClass::InvalidRequest,
        // A failure we built ourselves, or one whose cause is a retry loop
        // that already ran out: neither is a classification of the provider.
        ApiError::Http(_) | ApiError::RetryExhausted { .. } => ErrorClass::Unknown,
    }
}

/// The triage decision for a failure: its class, and whether to try again.
#[must_use]
pub const fn triage(error: &ApiError) -> ErrorTriage {
    let class = classify_error(error);
    let action = match class {
        ErrorClass::Timeout
        | ErrorClass::Dns
        | ErrorClass::Connection
        | ErrorClass::Stream
        | ErrorClass::Server => RetryAction::Retry,
        ErrorClass::RateLimit => RetryAction::RetryAfterRateLimit,
        ErrorClass::Ssl
        | ErrorClass::Decode
        | ErrorClass::Auth
        | ErrorClass::AccessPolicy
        | ErrorClass::InvalidRequest
        | ErrorClass::Unknown => RetryAction::Fail,
    };
    ErrorTriage { class, action }
}

/// The floor for the backoff after a rate limit.
///
/// The provider has said to slow down; the exponential schedule alone would
/// come back too soon.
pub const RATE_LIMIT_BACKOFF_FLOOR: Duration = Duration::from_secs(5);

/// The delay before the next attempt, from the classification.
///
/// `attempt` is 1 for the first retry. The exponential schedule is unchanged
/// from before — only a rate limit's floor is new.
#[must_use]
pub fn retry_backoff_for(attempt: u32, base: Duration, triage: &ErrorTriage) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    let exponential = base.saturating_mul(1u32 << shift);
    match triage.action {
        RetryAction::RetryAfterRateLimit => exponential.max(RATE_LIMIT_BACKOFF_FLOOR),
        RetryAction::Retry | RetryAction::Fail => exponential,
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        // Asked of the error itself rather than read out of its message.
        let class = classify_transport_facts(TransportFacts {
            timeout: e.is_timeout(),
            body: e.is_body(),
            decode: e.is_decode(),
            connect: e.is_connect(),
            request: e.is_request(),
            io_kind: io_error_kind(&e),
        });
        Self::Transport {
            kind: class.transport_kind(),
            class,
            message: e.to_string(),
        }
    }
}

/// How much reasoning a provider should spend on a request.
///
/// The value is provider-neutral: each provider maps it onto whatever control
/// its API actually exposes (an Anthropic thinking budget, an OpenAI-compatible
/// `reasoning_effort` field). A provider with no equivalent omits it and says
/// so rather than silently dropping the request — see
/// [`ResolvedProviderProfile::reasoning_effort_support`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Spend as little as possible.
    Low,
    /// The provider's middle setting.
    Medium,
    /// Spend as much as the provider allows.
    High,
}

impl ReasoningEffort {
    /// The wire spelling accepted by the CLI and by OpenAI-style APIs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Parse the CLI spelling, case-insensitively.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    /// The Anthropic extended-thinking budget for this level.
    ///
    /// Anthropic requires a budget of at least 1024 tokens, so `Low` is the
    /// floor rather than something smaller.
    #[must_use]
    pub const fn anthropic_thinking_budget(self) -> u32 {
        match self {
            Self::Low => 1024,
            Self::Medium => 4096,
            Self::High => 16_384,
        }
    }
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| {
            format!("invalid reasoning effort '{value}': expected one of low, medium, high")
        })
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
    /// Default reasoning effort for requests made with this client.
    ///
    /// A per-request value on [`RequestContext`] wins over this default; see
    /// [`ApiConfig::effective_reasoning_effort`].
    pub reasoning_effort: Option<ReasoningEffort>,
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

/// The `provider_hints` key that carries [`ReasoningEffort`].
///
/// The effort rides in the existing hint map so it reaches providers through
/// the plumbing that already propagates [`RequestContext`] end to end, without
/// widening the struct (and every literal that builds it).
pub const REASONING_EFFORT_HINT: &str = "reasoning_effort";

impl RequestContext {
    /// The reasoning effort requested for this turn, if any.
    #[must_use]
    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.provider_hints
            .get(REASONING_EFFORT_HINT)
            .and_then(|value| ReasoningEffort::parse(value))
    }

    /// Request a reasoning effort for this turn.
    pub fn set_reasoning_effort(&mut self, effort: ReasoningEffort) {
        self.provider_hints.insert(
            REASONING_EFFORT_HINT.to_string(),
            effort.as_str().to_string(),
        );
    }
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
            reasoning_effort: None,
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

    /// Set the default reasoning effort for this client.
    #[must_use]
    pub const fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    /// The reasoning effort to apply to a request.
    ///
    /// A value carried on the per-request [`RequestContext`] wins over the
    /// client default, because the caller that built the context is the one
    /// that knows about *this* turn.
    #[must_use]
    pub fn effective_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.request_context
            .as_ref()
            .and_then(RequestContext::reasoning_effort)
            .or(self.reasoning_effort)
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

/// An [`ApiClient`] that stamps a reasoning effort onto every request context.
///
/// The engine builds a fresh [`RequestContext`] per turn and knows nothing
/// about `--effort`; this wrapper sits in front of the provider router and adds
/// the effort to each context as it goes past. It does not overwrite an effort
/// a caller already set, so a future per-turn override still wins.
pub struct EffortClient {
    inner: Box<dyn ApiClient>,
    effort: ReasoningEffort,
}

impl EffortClient {
    /// Wrap `inner` so every request carries `effort`.
    #[must_use]
    pub fn new(inner: Box<dyn ApiClient>, effort: ReasoningEffort) -> Self {
        Self { inner, effort }
    }

    /// The effort this client stamps onto requests.
    #[must_use]
    pub const fn effort(&self) -> ReasoningEffort {
        self.effort
    }

    /// The context handed to the inner client.
    fn context_with_effort(&self, mut context: RequestContext) -> RequestContext {
        if context.reasoning_effort().is_none() {
            context.set_reasoning_effort(self.effort);
        }
        context
    }
}

impl Clone for EffortClient {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone_box(),
            effort: self.effort,
        }
    }
}

impl std::fmt::Debug for EffortClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffortClient")
            .field("effort", &self.effort)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl ApiClient for EffortClient {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(self.clone())
    }

    fn with_request_context(&self, context: RequestContext) -> Box<dyn ApiClient> {
        Box::new(Self {
            inner: self
                .inner
                .with_request_context(self.context_with_effort(context)),
            effort: self.effort,
        })
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
    pub const fn new_with_client(config: ApiConfig, client: Client) -> Self {
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

    /// How this endpoint can express a reasoning effort.
    ///
    /// A cloud deployment is its own case: Bedrock, Vertex, and the other cloud
    /// kinds all speak the Anthropic Messages shape here, so they take the
    /// extended-thinking budget even though their host name resolves to a
    /// profile CodeRex does not otherwise know.
    fn reasoning_effort_support(&self) -> ReasoningEffortSupport {
        if self.config.cloud.is_some() {
            return ReasoningEffortSupport::AnthropicThinking;
        }
        self.resolved_profile().reasoning_effort_support()
    }

    /// Apply a reasoning effort to the request body, when this endpoint has an
    /// equivalent for it.
    ///
    /// Anthropic-style endpoints express effort as extended thinking: a
    /// `thinking.budget_tokens` value. Anthropic rejects a body whose
    /// `max_tokens` is not above that budget, so the ceiling is raised when the
    /// caller's is too low. An endpoint with no equivalent is left untouched
    /// and a warning says so — the request is not silently reshaped.
    fn apply_reasoning_effort(&self, body: &mut serde_json::Value, max_tokens: u32) {
        let Some(effort) = self.config.effective_reasoning_effort() else {
            return;
        };
        match self.reasoning_effort_support() {
            ReasoningEffortSupport::AnthropicThinking => {
                let budget = effort.anthropic_thinking_budget();
                if let Some(object) = body.as_object_mut() {
                    object.insert(
                        "thinking".into(),
                        serde_json::json!({"type": "enabled", "budget_tokens": budget}),
                    );
                    if max_tokens <= budget {
                        object.insert("max_tokens".into(), serde_json::json!(budget + 1024));
                    }
                }
            }
            ReasoningEffortSupport::OpenAiReasoningEffort | ReasoningEffortSupport::Unsupported => {
                tracing::warn!(
                    provider = self.profile_name(),
                    effort = %effort,
                    "provider has no reasoning-effort control; the request omits it"
                );
            }
        }
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
                req.header("Authorization", format!("Bearer {}", self.config.api_key))
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
                        req.header("Authorization", format!("Bearer {}", self.config.api_key))
                    }
                    None => req.header("Authorization", format!("Bearer {}", self.config.api_key)),
                }
            }
            AuthStyle::AzureApiKey => req.header("api-key", &self.config.api_key),
            AuthStyle::GoogleAccessToken | AuthStyle::Bearer => {
                req.header("Authorization", format!("Bearer {}", self.config.api_key))
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
        request_id: &str,
        server_request_id: Option<&str>,
        message: &str,
        fallback_reason: Option<&str>,
    ) -> String {
        use std::fmt::Write as _;

        let mut diagnostics = format!("request_id={request_id}");
        if let Some(server_request_id) = server_request_id.filter(|id| !id.is_empty()) {
            let _ = write!(diagnostics, ", server_request_id={server_request_id}");
        }
        if let Some(fallback_reason) = fallback_reason {
            let _ = write!(diagnostics, ", fallback_reason={fallback_reason}");
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
        let mut last_error: Option<ApiError> = None;
        for attempt in 0..=self.config.max_retries {
            if attempt > 0 {
                // The backoff follows the classification: a rate limit backs
                // off further than a dropped connection.
                let previous = last_error.as_ref().map_or(
                    ErrorTriage {
                        class: ErrorClass::Unknown,
                        action: RetryAction::Retry,
                    },
                    triage,
                );
                let backoff = retry_backoff_for(attempt, self.config.retry_backoff, &previous);
                tracing::debug!(
                    "API retry {attempt}/{} after {} — backing off {backoff:?}",
                    self.config.max_retries,
                    previous.class
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
            last_error_message: last_error.map_or_else(|| "unknown".into(), |e| e.to_string()),
        })
    }
}

#[async_trait::async_trait]
impl ApiClient for AnthropicApiClient {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(Self {
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
        self.apply_reasoning_effort(&mut body, max_tokens);

        let capabilities = self.resolved_profile().capabilities;
        let tool_fallback = !tools.is_empty() && !capabilities.tool_use;
        self.log_request_start(
            &request_id,
            model_name,
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
                    let body = resp.text().await.unwrap_or_default();
                    let err_text = Self::format_error_message(
                        &request_id,
                        server_request_id.as_deref(),
                        &body,
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
        self.apply_reasoning_effort(&mut body, max_tokens);

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
                let temp_client = Self {
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
                            let body = resp.text().await.unwrap_or_default();
                            let err_text = Self::format_error_message(
                                &request_id,
                                server_request_id.as_deref(),
                                &body,
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

    let (authority, rest) = without_scheme
        .find('/')
        .map_or((without_scheme, ""), |index| {
            (&without_scheme[..index], &without_scheme[index..])
        });

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
    let Some(json_str) = data_line.strip_prefix("data:").map(str::trim_start) else {
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
                .and_then(|i| usize::try_from(i).ok())
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
                .and_then(|i| usize::try_from(i).ok())
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
                    if let Some(id) = state.index_to_id.get(&index) {
                        vec![StreamEvent::ToolCallInputDelta {
                            id: id.clone(),
                            delta: partial_json,
                        }]
                    } else {
                        // Mapping not yet established: buffer the delta
                        // until `content_block_start` registers the index.
                        state.deferred_deltas.push((index, partial_json));
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            }
        }
        "content_block_stop" => {
            let Some(index) = value
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .and_then(|i| usize::try_from(i).ok())
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
/// Classify an error as retryable (transient): timeouts, name-resolution and
/// connection failures, cut-off streams, server errors and rate limits.
/// Auth/policy, TLS, decode and other 4xx errors are not.
///
/// This is the triage table's decision — there is one place that decides, so
/// the retry rule and the backoff rule cannot drift apart.
#[must_use]
pub const fn is_retryable(error: &ApiError) -> bool {
    triage(error).is_retry()
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
    std::env::var("CODEREX_CUSTOM_HEADERS")
        .map_or_else(|_| BTreeMap::new(), |value| parse_custom_headers(&value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_parses_and_round_trips() {
        assert_eq!(ReasoningEffort::parse("low"), Some(ReasoningEffort::Low));
        assert_eq!(ReasoningEffort::parse("HIGH"), Some(ReasoningEffort::High));
        assert_eq!(
            ReasoningEffort::parse(" medium "),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(ReasoningEffort::parse("maximum"), None);
        assert_eq!(ReasoningEffort::High.as_str(), "high");
        assert_eq!(
            "medium".parse::<ReasoningEffort>(),
            Ok(ReasoningEffort::Medium)
        );

        // The enum is serde-friendly, so a plugin or config file can carry it.
        let json = serde_json::to_string(&ReasoningEffort::Low).expect("serializes");
        assert_eq!(json, "\"low\"");
        let parsed: ReasoningEffort = serde_json::from_str("\"high\"").expect("deserializes");
        assert_eq!(parsed, ReasoningEffort::High);
    }

    #[test]
    fn anthropic_effort_becomes_a_thinking_budget() {
        let config = ApiConfig::new("k".into()).with_reasoning_effort(ReasoningEffort::High);
        let client = AnthropicApiClient::new(config);
        let mut body = serde_json::json!({"max_tokens": 1024});
        client.apply_reasoning_effort(&mut body, 1024);

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 16_384);
        // Anthropic rejects max_tokens <= budget_tokens, so the ceiling moves.
        assert_eq!(body["max_tokens"], 16_384 + 1024);
    }

    #[test]
    fn anthropic_keeps_a_max_tokens_above_the_budget() {
        let config = ApiConfig::new("k".into()).with_reasoning_effort(ReasoningEffort::Low);
        let client = AnthropicApiClient::new(config);
        let mut body = serde_json::json!({"max_tokens": 8192});
        client.apply_reasoning_effort(&mut body, 8192);
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert_eq!(
            body["max_tokens"], 8192,
            "a ceiling above the budget is left alone"
        );
    }

    #[test]
    fn effort_is_omitted_when_the_profile_has_no_equivalent() {
        let config = ApiConfig::new("k".into())
            .with_provider_profile("kimi")
            .with_reasoning_effort(ReasoningEffort::High);
        let client = AnthropicApiClient::new(config);
        let mut body = serde_json::json!({"max_tokens": 1024});
        client.apply_reasoning_effort(&mut body, 1024);
        // The field is omitted rather than sent to a provider that would
        // reject it; the warning is what makes that honest.
        assert!(body.get("thinking").is_none());
        assert_eq!(body["max_tokens"], 1024);
    }

    #[test]
    fn request_context_effort_wins_over_the_client_default() {
        let mut context = RequestContext::default();
        context.set_reasoning_effort(ReasoningEffort::Low);
        let config = ApiConfig::new("k".into())
            .with_reasoning_effort(ReasoningEffort::High)
            .with_request_context(context);
        assert_eq!(
            config.effective_reasoning_effort(),
            Some(ReasoningEffort::Low),
            "the per-turn context is more specific than the client default"
        );
    }

    #[test]
    fn effort_client_stamps_the_context_and_respects_an_existing_value() {
        let inner = AnthropicApiClient::new(ApiConfig::new("k".into()));
        let client = EffortClient::new(Box::new(inner), ReasoningEffort::High);

        let stamped = client.with_request_context(RequestContext::default());
        assert_eq!(client.effort(), ReasoningEffort::High);
        drop(stamped);

        // A caller that already chose an effort keeps it.
        let mut context = RequestContext::default();
        context.set_reasoning_effort(ReasoningEffort::Low);
        let unchanged = client.context_with_effort(context);
        assert_eq!(unchanged.reasoning_effort(), Some(ReasoningEffort::Low));
    }

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
        let transport = |class: ErrorClass, message: &str| ApiError::Transport {
            kind: class.transport_kind(),
            class,
            message: message.to_string(),
        };

        assert!(is_retryable(&transport(
            ErrorClass::Timeout,
            "something entirely different"
        )));
        assert!(is_retryable(&transport(ErrorClass::Connection, "")));

        // A message that mentions the words is not a timeout.
        assert!(!is_retryable(&transport(
            ErrorClass::Unknown,
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
            let class = match kind {
                TransportError::Timeout => ErrorClass::Timeout,
                TransportError::Connect => ErrorClass::Connection,
                TransportError::Other => ErrorClass::Unknown,
            };
            let error = ApiError::Transport {
                kind,
                class,
                message: String::new(),
            };
            assert_eq!(is_retryable(&error), retryable, "{kind}");
        }
    }

    // ─── Triage table ───────────────────────────────────────────────────────

    /// The triage table, one row per class. A permanent failure that is
    /// retried is the defect this table exists to prevent, so the failing
    /// rows matter as much as the retrying ones.
    #[test]
    fn triage_classifies_every_class_by_fact() {
        let transport = |class: ErrorClass| ApiError::Transport {
            kind: class.transport_kind(),
            class,
            message: String::new(),
        };

        let cases = [
            (
                transport(ErrorClass::Ssl),
                ErrorClass::Ssl,
                RetryAction::Fail,
            ),
            (
                transport(ErrorClass::Dns),
                ErrorClass::Dns,
                RetryAction::Retry,
            ),
            (
                transport(ErrorClass::Connection),
                ErrorClass::Connection,
                RetryAction::Retry,
            ),
            (
                transport(ErrorClass::Timeout),
                ErrorClass::Timeout,
                RetryAction::Retry,
            ),
            (
                transport(ErrorClass::Stream),
                ErrorClass::Stream,
                RetryAction::Retry,
            ),
            (
                transport(ErrorClass::Decode),
                ErrorClass::Decode,
                RetryAction::Fail,
            ),
            (
                ApiError::Response {
                    status: 429,
                    message: String::new(),
                },
                ErrorClass::RateLimit,
                RetryAction::RetryAfterRateLimit,
            ),
            (
                ApiError::Response {
                    status: 503,
                    message: String::new(),
                },
                ErrorClass::Server,
                RetryAction::Retry,
            ),
            (
                ApiError::Auth(String::new()),
                ErrorClass::Auth,
                RetryAction::Fail,
            ),
            (
                ApiError::AccessPolicy(String::new()),
                ErrorClass::AccessPolicy,
                RetryAction::Fail,
            ),
            (
                ApiError::Response {
                    status: 400,
                    message: String::new(),
                },
                ErrorClass::InvalidRequest,
                RetryAction::Fail,
            ),
            (
                ApiError::InvalidModel(String::new()),
                ErrorClass::InvalidRequest,
                RetryAction::Fail,
            ),
            (
                ApiError::Http(String::new()),
                ErrorClass::Unknown,
                RetryAction::Fail,
            ),
        ];

        for (error, class, action) in cases {
            let decision = triage(&error);
            assert_eq!(decision.class, class, "{error}");
            assert_eq!(decision.action, action, "{error}");
            assert_eq!(decision.is_retry(), !matches!(action, RetryAction::Fail));
        }
    }

    /// A TLS/certificate failure and a name that will not resolve are both
    /// "connect" failures to the HTTP client; the triage tells them apart by
    /// the `io::ErrorKind` underneath, not by the message.
    #[test]
    fn transport_facts_tell_tls_and_dns_from_a_refused_connection() {
        let connect = |io_kind| TransportFacts {
            connect: true,
            io_kind,
            ..TransportFacts::default()
        };

        assert_eq!(
            classify_transport_facts(connect(Some(std::io::ErrorKind::InvalidData))),
            ErrorClass::Ssl
        );
        assert_eq!(
            classify_transport_facts(connect(Some(std::io::ErrorKind::NotFound))),
            ErrorClass::Dns
        );
        assert_eq!(
            classify_transport_facts(connect(Some(std::io::ErrorKind::ConnectionRefused))),
            ErrorClass::Connection
        );
        assert_eq!(
            classify_transport_facts(connect(None)),
            ErrorClass::Connection
        );

        // A timeout is a timeout whatever else is also true.
        assert_eq!(
            classify_transport_facts(TransportFacts {
                timeout: true,
                connect: true,
                io_kind: Some(std::io::ErrorKind::InvalidData),
                ..TransportFacts::default()
            }),
            ErrorClass::Timeout
        );

        // A body that failed after a response arrived is a stream failure,
        // not a connect failure.
        assert_eq!(
            classify_transport_facts(TransportFacts {
                body: true,
                ..TransportFacts::default()
            }),
            ErrorClass::Stream
        );
        assert_eq!(
            classify_transport_facts(TransportFacts {
                decode: true,
                ..TransportFacts::default()
            }),
            ErrorClass::Decode
        );
        assert_eq!(
            classify_transport_facts(TransportFacts::default()),
            ErrorClass::Unknown
        );
    }

    /// A permanent failure must not be retried, and the class must not be
    /// recoverable by wording the message differently.
    #[test]
    fn a_permanent_failure_is_not_retried_however_it_is_worded() {
        for message in ["timeout", "connection reset", "please retry", "429"] {
            let ssl = ApiError::Transport {
                kind: TransportError::Other,
                class: ErrorClass::Ssl,
                message: message.to_string(),
            };
            assert!(!is_retryable(&ssl), "ssl must not be retried: {message}");

            let auth = ApiError::Auth(message.to_string());
            assert!(!is_retryable(&auth), "auth must not be retried: {message}");
        }
    }

    /// A rate limit backs off further than a dropped connection, and the
    /// schedule is still exponential.
    #[test]
    fn backoff_follows_the_classification() {
        let base = Duration::from_secs(1);
        let connection = triage(&ApiError::Transport {
            kind: TransportError::Connect,
            class: ErrorClass::Connection,
            message: String::new(),
        });
        let rate_limit = triage(&ApiError::Response {
            status: 429,
            message: String::new(),
        });

        assert_eq!(retry_backoff_for(1, base, &connection), base);
        assert_eq!(
            retry_backoff_for(2, base, &connection),
            Duration::from_secs(2)
        );
        assert_eq!(
            retry_backoff_for(3, base, &connection),
            Duration::from_secs(4)
        );

        // The rate-limit floor wins over a short exponential backoff.
        assert_eq!(
            retry_backoff_for(1, base, &rate_limit),
            RATE_LIMIT_BACKOFF_FLOOR
        );
        assert_eq!(
            retry_backoff_for(4, base, &rate_limit),
            Duration::from_secs(8)
        );
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
