//! HTTP and SSE JSON-RPC transport for remote MCP servers.

use crate::channel::{
    InboundRequestHandler, JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    JsonRpcServerRequest, METHOD_NOT_FOUND, ServerNotificationHandler,
};
use reqwest::Url;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, WWW_AUTHENTICATE,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

/// The header the MCP streamable-HTTP transport uses to name a session.
const SESSION_ID_HEADER: &str = "mcp-session-id";
/// The header that says which protocol revision a post-initialize request uses.
const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
/// The revision this client asks for and assumes when a server echoes none.
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
/// The header that resumes an SSE stream where it left off.
const LAST_EVENT_ID_HEADER: &str = "last-event-id";
/// The `Accept` value a standalone server-to-client stream requires.
const EVENT_STREAM_ACCEPT: &str = "text/event-stream";
/// How long a server request id is remembered after it was answered.
const DEDUP_TTL: Duration = Duration::from_secs(300);
/// The most answered request ids kept for retry detection.
const DEDUP_CAPACITY: usize = 256;
/// A single SSE event larger than this is not a JSON-RPC message; refusing it
/// keeps a broken or hostile stream from growing the read buffer without end.
const MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;
/// Messages the stream may hold while the handler is busy before it closes.
const INBOUND_QUEUE_CAPACITY: usize = 64;
/// How long a legacy-SSE request waits for its response on the event stream.
///
/// A streamable-HTTP response is bound to the POST that carried the request,
/// but a classic SSE server answers asynchronously on the long-lived GET
/// stream. Without a bound, a server that drops the message would leave the
/// caller waiting forever.
const LEGACY_SSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How often a degraded standalone stream probes for recovery.
///
/// The value is seconds. The default is
/// [`DEFAULT_STREAM_PROBE_INTERVAL`]; `0` disables recovery (the task stays
/// `Degraded`, as it did before the probe existed).
pub const MCP_STREAM_PROBE_ENV: &str = "CODEREX_MCP_STREAM_PROBE_SECS";
/// The probe interval used when [`MCP_STREAM_PROBE_ENV`] is unset.
pub const DEFAULT_STREAM_PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Senders waiting for the response to a legacy-SSE POST, keyed by JSON-RPC id.
type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// Read the degraded-stream probe interval from the environment.
///
/// `None` means recovery is disabled. A malformed value is logged and treated
/// as the default rather than as "disabled", so a typo cannot permanently
/// silence recovery.
fn stream_probe_interval_from_env() -> Option<Duration> {
    let value = match std::env::var(MCP_STREAM_PROBE_ENV) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Some(DEFAULT_STREAM_PROBE_INTERVAL),
        Err(error) => {
            tracing::warn!(
                variable = MCP_STREAM_PROBE_ENV,
                "could not read the MCP stream probe interval: {error}; using the default"
            );
            return Some(DEFAULT_STREAM_PROBE_INTERVAL);
        }
    };

    match value.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(seconds) => Some(Duration::from_secs(seconds)),
        Err(error) => {
            tracing::warn!(
                variable = MCP_STREAM_PROBE_ENV,
                value = %value,
                "invalid MCP stream probe interval: {error}; using the default"
            );
            Some(DEFAULT_STREAM_PROBE_INTERVAL)
        }
    }
}

/// The two remote transports this module speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteMode {
    /// MCP streamable HTTP: POST requests, optional standalone GET stream.
    StreamableHttp,
    /// Legacy MCP SSE: a long-lived GET event stream that announces the POST
    /// message endpoint, and responses correlated by JSON-RPC id.
    LegacySse,
}

/// Why one legacy-SSE request attempt failed.
#[derive(Debug)]
enum LegacyRequestFailure {
    /// The POST was rejected with an HTTP status.
    Status {
        /// The status the server returned.
        status: reqwest::StatusCode,
        /// The response body, for the error text.
        body: String,
    },
    /// The request never reached the server, or the endpoint was never
    /// announced.
    Transport(String),
    /// The server answered the request with a JSON-RPC error.
    Response(String),
}

impl LegacyRequestFailure {
    /// Render the failure for the caller.
    fn into_message(self) -> String {
        match self {
            Self::Status { status, body } => {
                format!("MCP SSE server returned {status}: {body}")
            }
            Self::Transport(message) | Self::Response(message) => message,
        }
    }
}

/// The state of a remote server's standalone server-to-client stream.
///
/// This is surfaced to the session so "the server cannot push messages to us"
/// is a visible fact rather than a task that retries forever in the
/// background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStreamState {
    /// No stream is running (never started, or shut down).
    Stopped,
    /// The first connection is being made.
    Starting,
    /// The stream is connected and delivering messages.
    Active,
    /// The retry budget was exhausted. When a probe interval is configured
    /// (see [`MCP_STREAM_PROBE_ENV`]) the task waits that interval and then
    /// makes one reconnect, returning to [`Self::Active`] if it succeeds.
    /// With the probe disabled the stream stays here until it is restarted.
    Degraded,
    /// The server answered 405: it does not offer a standalone stream. This is
    /// expected for many servers and is not an error.
    Unsupported,
}

/// How hard the standalone stream tries before it gives up.
#[derive(Debug, Clone, Copy)]
pub struct EventStreamRetryPolicy {
    /// Consecutive failed connections (without a delivered message) allowed.
    pub max_attempts: usize,
    /// The first delay between attempts; doubled on each failure.
    pub base_delay: Duration,
    /// The delay cap.
    pub max_delay: Duration,
    /// The longest the stream may keep retrying without progress.
    pub max_duration: Duration,
}

impl Default for EventStreamRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(5),
            max_duration: Duration::from_secs(30),
        }
    }
}

/// The identity of a server request: a request id is only unique within one
/// session, and a session can be re-initialized.
type RequestKey = (String, u64);

/// Whether a server request is being answered or was answered recently.
#[derive(Debug, Clone, Copy)]
enum DedupEntry {
    /// A handler is running right now.
    Pending,
    /// The answer was delivered; a redelivery must not ask again.
    Answered,
}

/// Recent server-request keys, used to answer each request exactly once even
/// when the same message arrives on both the POST and the standalone stream.
#[derive(Debug, Default)]
struct RequestDedup {
    inner: Mutex<DedupInner>,
}

#[derive(Debug, Default)]
struct DedupInner {
    entries: HashMap<RequestKey, (DedupEntry, Instant)>,
    order: VecDeque<RequestKey>,
}

impl RequestDedup {
    /// Claim `key` for this attempt. Returns false for a pending or recent
    /// duplicate.
    fn begin(&self, key: &RequestKey) -> bool {
        let now = Instant::now();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        while let Some(oldest) = inner.order.front() {
            let expired = inner
                .entries
                .get(oldest)
                .is_some_and(|(_, at)| now.duration_since(*at) > DEDUP_TTL);
            if !expired {
                break;
            }
            if let Some(key) = inner.order.pop_front() {
                inner.entries.remove(&key);
            }
        }

        if inner.entries.contains_key(key) {
            return false;
        }
        while inner.entries.len() >= DEDUP_CAPACITY {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            inner.entries.remove(&oldest);
        }
        inner
            .entries
            .insert(key.clone(), (DedupEntry::Pending, now));
        inner.order.push_back(key.clone());
        true
    }

    /// Mark a claimed request as answered.
    fn complete(&self, key: &RequestKey) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .entries
            .insert(key.clone(), (DedupEntry::Answered, Instant::now()));
    }

    /// Release a claim whose answer could not be delivered, so a retry can
    /// answer it.
    fn fail(&self, key: &RequestKey) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.entries.remove(key);
        inner.order.retain(|entry| entry != key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .len()
    }
}

/// Supplies the credential for a remote request.
///
/// It is consulted before *every* HTTP request, not once at startup: a session
/// can outlive its access token, and the provider is where a near-expiry token
/// is refreshed. Returning `None` means "send no credential"; the channel then
/// falls back to whatever static headers it was configured with, which is the
/// same behaviour as a server that has never signed in.
///
/// The method is async because a refresh is a network round trip. An
/// implementation must not hold a lock that another request needs while doing
/// unrelated work.
#[async_trait::async_trait]
pub trait TokenProvider: Send + Sync + std::fmt::Debug {
    /// The bearer token to attach, if one is available.
    async fn access_token(&self) -> Option<String>;

    /// Recover from a `401` whose bearer challenge says `invalid_token`.
    ///
    /// `rejected_token` is the token the failed request actually carried, so a
    /// provider can tell a stale 401 from one that raced with a refresh. The
    /// returned token is used for exactly one retry; `Ok(None)` means "no
    /// recovery available" and the original 401 is surfaced unchanged.
    ///
    /// The default keeps the historical behaviour for plain bearer providers:
    /// a 401 is an ordinary error.
    async fn handle_unauthorized(
        &self,
        _www_authenticate: Option<&str>,
        _rejected_token: Option<&str>,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// The configuration a remote request needs.
///
/// Cloneable and owned so the standalone GET stream can run in its own task
/// while every POST uses the same session id and token provider. Cloning does
/// not copy the credential or the session: those are resolved at request time
/// from the shared state.
#[derive(Debug, Clone)]
struct RequestContext {
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    /// Resolves the bearer token before each request, when installed.
    token_provider: Option<Arc<dyn TokenProvider>>,
    /// The server's session id, learned from an initialize response header.
    session_id: Arc<RwLock<Option<String>>>,
    /// The protocol revision the server negotiated in its initialize result.
    protocol_version: Arc<RwLock<Option<String>>>,
    /// Server requests already answered (or being answered) over either stream.
    dedup: Arc<RequestDedup>,
    /// The POST message endpoint announced by a legacy-SSE `endpoint` event.
    ///
    /// Always `None` for streamable HTTP, whose requests go to `url`. A
    /// `watch` channel rather than a plain lock: requests must wait for the
    /// first `endpoint` event without a poll loop, and a reconnect may replace
    /// it.
    message_url: Arc<watch::Sender<Option<Url>>>,
}

impl RequestContext {
    fn session_id(&self) -> Option<String> {
        self.session_id.read().ok().and_then(|slot| slot.clone())
    }

    /// Forget everything tied to the old session. A re-initialize negotiates
    /// both values again.
    fn clear_session(&self) {
        if let Ok(mut slot) = self.session_id.write() {
            *slot = None;
        }
        if let Ok(mut slot) = self.protocol_version.write() {
            *slot = None;
        }
    }

    /// The URL a JSON-RPC message is POSTed to.
    ///
    /// Streamable HTTP always uses the configured endpoint. Legacy SSE uses
    /// the message endpoint announced by the event stream once it is known.
    fn post_target(&self) -> Url {
        self.message_url
            .borrow()
            .clone()
            .unwrap_or_else(|| self.url.clone())
    }

    /// Remember the message endpoint announced by a legacy-SSE `endpoint`
    /// event.
    fn set_message_url(&self, url: Url) {
        self.message_url.send_replace(Some(url));
    }

    /// Forget the announced message endpoint, e.g. before reconnecting.
    fn clear_message_url(&self) {
        self.message_url.send_replace(None);
    }

    /// Wait until the legacy-SSE stream has announced its POST endpoint.
    async fn wait_for_message_url(&self) -> Result<Url, String> {
        let current = self.message_url.borrow().clone();
        if let Some(url) = current {
            return Ok(url);
        }

        let mut receiver = self.message_url.subscribe();
        let wait = async {
            loop {
                let current = receiver.borrow().clone();
                if let Some(url) = current {
                    return Ok(url);
                }
                receiver.changed().await.map_err(|_| {
                    "the MCP SSE stream ended before it announced a message endpoint".to_string()
                })?;
            }
        };

        tokio::time::timeout(LEGACY_SSE_REQUEST_TIMEOUT, wait)
            .await
            .map_err(|_| {
                format!(
                    "timed out after {}s waiting for the MCP SSE endpoint event",
                    LEGACY_SSE_REQUEST_TIMEOUT.as_secs()
                )
            })?
    }

    /// Remember a session id the server set on a response.
    fn capture_session_id(&self, headers: &HeaderMap) {
        let Some(value) = headers.get(SESSION_ID_HEADER) else {
            return;
        };
        let Ok(value) = value.to_str() else {
            tracing::warn!("MCP server sent a non-UTF-8 Mcp-Session-Id header");
            return;
        };
        let value = value.trim();
        if !value.is_empty() {
            if let Ok(mut slot) = self.session_id.write() {
                *slot = Some(value.to_string());
            }
        }
    }

    /// Remember the protocol revision the initialize result negotiated.
    fn capture_protocol_version(&self, result: &Value) {
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION)
            .trim();
        if !version.is_empty() {
            if let Ok(mut slot) = self.protocol_version.write() {
                *slot = Some(version.to_string());
            }
        }
    }

    fn protocol_version(&self) -> Option<String> {
        self.protocol_version
            .read()
            .ok()
            .and_then(|slot| slot.clone())
    }

    /// The headers for one request plus the bearer token that went into them.
    ///
    /// The token is returned so a 401 can name the credential the request
    /// carried. It is `None` when the provider had nothing (or there is no
    /// provider), which is also the case where a configured static
    /// `Authorization` header is left in place.
    async fn resolved_headers(&self) -> (HeaderMap, Option<String>) {
        let token = match &self.token_provider {
            Some(provider) => provider.access_token().await,
            None => None,
        };
        (self.request_headers_with_token(token.clone()), token)
    }

    /// Headers for one request with an explicit token.
    ///
    /// Used for the single retry after a refresh: it must carry the token the
    /// provider just returned, not ask the provider again (which could refresh
    /// a second time).
    fn request_headers_with_token(&self, token: Option<String>) -> HeaderMap {
        let mut headers = self.headers.clone();
        if let Some(token) = token {
            match HeaderValue::from_str(&format!("Bearer {token}")) {
                Ok(value) => {
                    headers.insert(AUTHORIZATION, value);
                }
                Err(error) => {
                    tracing::warn!("MCP OAuth token cannot be sent as an HTTP header: {error}");
                }
            }
        }
        if let Some(session_id) = self.session_id() {
            match HeaderValue::from_str(&session_id) {
                Ok(value) => {
                    headers.insert(HeaderName::from_static(SESSION_ID_HEADER), value);
                }
                Err(error) => {
                    tracing::warn!("MCP session id cannot be sent as an HTTP header: {error}");
                }
            }
        }
        if let Some(version) = self.protocol_version() {
            match HeaderValue::from_str(&version) {
                Ok(value) => {
                    headers.insert(HeaderName::from_static(PROTOCOL_VERSION_HEADER), value);
                }
                Err(error) => {
                    tracing::warn!(
                        "MCP protocol version cannot be sent as an HTTP header: {error}"
                    );
                }
            }
        }
        headers
    }

    /// Send one request, recovering once from a stale-token `401`.
    ///
    /// Recovery is attempted only for a `401` carrying
    /// `WWW-Authenticate: Bearer ... error="invalid_token"`, which is the one
    /// challenge that says the access token is stale. `insufficient_scope` and
    /// every other 401 are returned unchanged so the caller surfaces the
    /// ordinary error without spending the refresh token. A second 401 after
    /// the retry is an error; there is no loop.
    async fn send_with_recovery<F>(
        &self,
        context: &str,
        build: F,
    ) -> Result<reqwest::Response, String>
    where
        F: Fn(HeaderMap) -> reqwest::RequestBuilder,
    {
        let (headers, token) = self.resolved_headers().await;
        let response = build(headers)
            .send()
            .await
            .map_err(|error| format!("{context}: {error}"))?;

        let Some(challenge) = invalid_token_challenge(&response) else {
            return Ok(response);
        };
        let Some(provider) = &self.token_provider else {
            return Ok(response);
        };

        let token = provider
            .handle_unauthorized(Some(&challenge), token.as_deref())
            .await
            .map_err(|error| format!("{context}: {error}"))?;
        let Some(token) = token else {
            // No recovery is available; the caller reports the 401 honestly.
            return Ok(response);
        };

        let retried = build(self.request_headers_with_token(Some(token)))
            .send()
            .await
            .map_err(|error| format!("{context}: {error}"))?;
        if retried.status() == reqwest::StatusCode::UNAUTHORIZED {
            let status = retried.status();
            let body = retried.text().await.unwrap_or_default();
            return Err(format!(
                "{context}: still {status} after refreshing the access token: {body}"
            ));
        }
        Ok(retried)
    }

    /// POST one JSON-RPC message, remembering any session id the answer sets.
    async fn post<T: Serialize + Sync>(
        &self,
        body: &T,
        accept: &'static str,
    ) -> Result<reqwest::Response, String> {
        let url = self.post_target();
        let response = self
            .send_with_recovery("MCP HTTP request failed", |headers| {
                self.client
                    .post(url.clone())
                    .headers(headers)
                    .header(ACCEPT, accept)
                    .header(CONTENT_TYPE, "application/json")
                    .json(body)
            })
            .await?;
        self.capture_session_id(response.headers());
        Ok(response)
    }

    /// Answer a request the server made, over a new POST.
    async fn answer_server_request(
        &self,
        request: &JsonRpcServerRequest,
        handler: Option<&dyn InboundRequestHandler>,
    ) -> Result<(), String> {
        let answer = match handler {
            Some(handler) => {
                handler
                    .handle(&request.method, request.params.as_ref())
                    .await
            }
            None => Err(JsonRpcError {
                code: METHOD_NOT_FOUND,
                message: format!(
                    "this client has no handler for '{}'; nothing here can answer it",
                    request.method
                ),
                data: None,
            }),
        };

        let response = match answer {
            Ok(result) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(result),
                error: None,
            },
            Err(error) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: None,
                error: Some(error),
            },
        };

        let context = format!("failed to answer the server's '{}' request", request.method);
        let url = self.post_target();
        let delivered = self
            .send_with_recovery(&context, |headers| {
                self.client
                    .post(url.clone())
                    .headers(headers)
                    .header(CONTENT_TYPE, "application/json")
                    .json(&response)
            })
            .await?;

        let status = delivered.status();
        if !status.is_success() {
            let body = delivered.text().await.unwrap_or_default();
            return Err(format!(
                "the server rejected the answer to '{}' with {status}: {body}",
                request.method
            ));
        }
        Ok(())
    }

    /// Answer a server request unless this exact request has already been
    /// answered (or is being answered) for the current session.
    ///
    /// The same request can arrive twice — once carried by the POST response
    /// stream and once by the standalone GET stream, or again after a
    /// reconnect. The key is `(session_id, request_id)`; the handler is run
    /// once and the duplicate is dropped.
    async fn answer_once(
        &self,
        request: &JsonRpcServerRequest,
        handler: Option<&dyn InboundRequestHandler>,
    ) -> Result<(), String> {
        let key = (self.session_id().unwrap_or_default(), request.id);
        if !self.dedup.begin(&key) {
            tracing::debug!(
                id = request.id,
                method = %request.method,
                "skipping a duplicate server request"
            );
            return Ok(());
        }

        match self.answer_server_request(request, handler).await {
            Ok(()) => {
                self.dedup.complete(&key);
                Ok(())
            }
            Err(error) => {
                // The answer was not delivered, so a retry must be allowed to
                // try again rather than being mistaken for a duplicate.
                self.dedup.fail(&key);
                Err(error)
            }
        }
    }

    async fn send_notification(&self, notification: &JsonRpcNotification) -> Result<(), String> {
        let response = self
            .post(notification, "application/json, text/event-stream")
            .await?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = response.text().await.unwrap_or_default();
            Err(format!(
                "MCP HTTP server returned {status} for a notification: {body}"
            ))
        }
    }
}

/// A remote JSON-RPC channel. Requests always use POST; responses may be a
/// single JSON document or an SSE stream containing JSON-RPC messages.
///
/// After initialization the channel also opens the transport's standalone GET
/// stream, so a server can push requests and notifications at any time rather
/// than only while it is answering a POST.
#[derive(Debug)]
pub struct HttpChannel {
    context: RequestContext,
    /// Streamable HTTP or classic SSE.
    mode: RemoteMode,
    /// The handler shared with the GET-stream task, per event.
    ///
    /// Shared rather than copied so a handler installed after the stream
    /// started (the session's UI slot is one) is seen by later messages.
    event_handler: Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
    /// Notifications (`tools/list_changed`, `resources/updated`, …) from
    /// either stream, delivered to the manager so it can act on them.
    notification_handler: Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    /// The one background GET-stream task for this server, if it is running.
    event_stream: Mutex<Option<EventStreamHandle>>,
    /// The stream's current state, readable while the task runs.
    event_state: Arc<RwLock<EventStreamState>>,
    /// Retry settings for the standalone stream.
    retry_policy: EventStreamRetryPolicy,
    /// How often a degraded stream tries one reconnect before waiting again.
    /// `None` leaves it `Degraded` until the process restarts.
    stream_probe_interval: Option<Duration>,
    /// Legacy SSE responses waiting for their JSON-RPC id on the event stream.
    pending: PendingRequests,
    /// Keeps the legacy message-endpoint watch channel open while no request
    /// is waiting on it.
    _message_url_receiver: watch::Receiver<Option<Url>>,
}

/// A running standalone-stream task together with its cancellation token.
#[derive(Debug)]
struct EventStreamHandle {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl HttpChannel {
    /// A streamable-HTTP channel.
    pub fn new(url: &str, headers: HashMap<String, String>) -> Result<Self, String> {
        Self::new_with_mode(url, headers, RemoteMode::StreamableHttp)
    }

    /// A legacy (HTTP+SSE) channel: a long-lived GET event stream plus a
    /// separate POST message endpoint announced by that stream.
    pub fn new_sse(url: &str, headers: HashMap<String, String>) -> Result<Self, String> {
        Self::new_with_mode(url, headers, RemoteMode::LegacySse)
    }

    fn new_with_mode(
        url: &str,
        headers: HashMap<String, String>,
        mode: RemoteMode,
    ) -> Result<Self, String> {
        let url = Url::parse(url).map_err(|error| format!("invalid MCP server URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!(
                "unsupported MCP server URL scheme '{}'; expected http or https",
                url.scheme()
            ));
        }

        let mut header_map = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| format!("invalid HTTP header name: {error}"))?;
            let value = HeaderValue::from_str(&value)
                .map_err(|error| format!("invalid value for HTTP header {name}: {error}"))?;
            header_map.insert(name, value);
        }

        let (message_url, message_url_receiver) = watch::channel(None);
        Ok(Self {
            context: RequestContext {
                client: reqwest::Client::new(),
                url,
                headers: header_map,
                token_provider: None,
                session_id: Arc::new(RwLock::new(None)),
                protocol_version: Arc::new(RwLock::new(None)),
                dedup: Arc::new(RequestDedup::default()),
                message_url: Arc::new(message_url),
            },
            mode,
            event_handler: Arc::new(RwLock::new(None)),
            notification_handler: Arc::new(RwLock::new(None)),
            event_stream: Mutex::new(None),
            event_state: Arc::new(RwLock::new(EventStreamState::Stopped)),
            retry_policy: EventStreamRetryPolicy::default(),
            stream_probe_interval: stream_probe_interval_from_env(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            _message_url_receiver: message_url_receiver,
        })
    }

    /// Whether this channel speaks legacy HTTP+SSE rather than streamable HTTP.
    #[must_use]
    pub fn is_legacy_sse(&self) -> bool {
        self.mode == RemoteMode::LegacySse
    }

    /// Change how hard the standalone stream retries before it degrades.
    ///
    /// Exposed mainly so tests can exercise the cap without waiting out the
    /// production policy.
    pub const fn set_event_stream_retry_policy(&mut self, policy: EventStreamRetryPolicy) {
        self.retry_policy = policy;
    }

    /// Change how often a degraded standalone stream probes for recovery.
    ///
    /// `None` disables the probe; the stream stays `Degraded` until it is
    /// restarted. The value is read by the task when it enters the degraded
    /// state, so setting it before [`Self::start_event_stream`] is what
    /// matters. The default comes from [`MCP_STREAM_PROBE_ENV`].
    pub const fn set_event_stream_probe_interval(&mut self, interval: Option<Duration>) {
        self.stream_probe_interval = interval;
    }

    /// Install the sink for server notifications.
    pub fn set_notification_handler(&self, handler: Arc<dyn ServerNotificationHandler>) {
        if let Ok(mut slot) = self.notification_handler.write() {
            *slot = Some(handler);
        }
    }

    /// Install the credential resolver used for every subsequent request.
    pub fn set_token_provider(&mut self, provider: Arc<dyn TokenProvider>) {
        self.context.token_provider = Some(provider);
    }

    /// Install the handler the GET stream uses for server requests.
    ///
    /// The stream reads this slot per message, so a handler installed after
    /// the stream started still answers later requests.
    pub fn set_event_stream_handler(&self, handler: Arc<dyn InboundRequestHandler>) {
        if let Ok(mut slot) = self.event_handler.write() {
            *slot = Some(handler);
        }
    }

    /// The session id the server assigned, once it has answered an initialize.
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.context.session_id()
    }

    /// Send a request and wait for its response.
    ///
    /// The protocol runs both ways over streamable HTTP too: an SSE response
    /// stream may carry a request from the server (elicitation is the common
    /// one) before the response to our request. Those are answered as they
    /// arrive — with a separate POST, since the stream is still open and the
    /// server may be waiting — so a server that asks a question is never left
    /// waiting for an answer that is not coming.
    ///
    /// A session the server no longer recognises answers 404 or 405. When that
    /// happens with a session id set, the id is dropped and the handshake is
    /// run once more before the request is retried.
    pub async fn send_request(
        &self,
        request: &JsonRpcRequest,
        handler: Option<&dyn InboundRequestHandler>,
    ) -> Result<Value, String> {
        if self.is_legacy_sse() {
            return self.send_request_legacy(request, handler).await;
        }

        let mut reinitialized = false;
        loop {
            let response = self
                .context
                .post(request, "application/json, text/event-stream")
                .await?;
            let status = response.status();

            // 404 is the streamable-HTTP signal that the session expired; 405
            // is not: it belongs to the standalone GET stream and means the
            // server offers no such stream.
            let session_expired = status.as_u16() == 404
                && request.method != "initialize"
                && self.context.session_id().is_some();
            if session_expired {
                let body = response.text().await.unwrap_or_default();
                if reinitialized {
                    return Err(format!(
                        "MCP HTTP server returned {status} after re-initializing: {body}"
                    ));
                }
                tracing::debug!(
                    method = %request.method,
                    status = %status,
                    "the MCP session is no longer valid; clearing it and re-initializing"
                );
                self.context.clear_session();
                reinitialize(&self.context, handler).await?;
                reinitialized = true;
                continue;
            }

            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(format!("MCP HTTP server returned {status}: {body}"));
            }

            let result = finish_response(
                &self.context,
                &self.notification_handler,
                response,
                request.id,
                handler,
            )
            .await?;
            if request.method == "initialize" {
                self.context.capture_protocol_version(&result);
            }
            return Ok(result);
        }
    }

    /// Send one request over legacy HTTP+SSE and wait for its response on the
    /// event stream.
    ///
    /// A 404 to the POST means the session (and with it the announced message
    /// endpoint) is gone: the session id is cleared, a fresh event stream is
    /// opened, the handshake runs once more, and the request is retried. A 405
    /// is *not* a session expiry — it is an ordinary HTTP failure and is
    /// surfaced as such.
    async fn send_request_legacy(
        &self,
        request: &JsonRpcRequest,
        handshake_handler: Option<&dyn InboundRequestHandler>,
    ) -> Result<Value, String> {
        let mut reinitialized = false;
        loop {
            match self.send_legacy_once(request).await {
                Ok(value) => {
                    if request.method == "initialize" {
                        self.context.capture_protocol_version(&value);
                    }
                    return Ok(value);
                }
                Err(LegacyRequestFailure::Status { status, body })
                    if status == reqwest::StatusCode::NOT_FOUND
                        && request.method != "initialize" =>
                {
                    if reinitialized {
                        return Err(format!(
                            "MCP SSE server returned {status} after re-initializing: {body}"
                        ));
                    }
                    tracing::debug!(
                        method = %request.method,
                        "the MCP SSE session is no longer valid; opening a new stream and re-initializing"
                    );
                    self.context.clear_session();
                    self.context.clear_message_url();
                    // A classic SSE session lives on its event stream, so the
                    // new handshake needs a new stream and endpoint event.
                    self.stop_event_stream().await;
                    self.start_event_stream();
                    self.reinitialize_legacy(handshake_handler).await?;
                    reinitialized = true;
                }
                Err(error) => return Err(error.into_message()),
            }
        }
    }

    /// Re-run the initialize handshake over an already-restarted legacy-SSE
    /// stream.
    async fn reinitialize_legacy(
        &self,
        _handshake_handler: Option<&dyn InboundRequestHandler>,
    ) -> Result<(), String> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            // The application request id space starts at 1; 0 cannot collide.
            id: 0,
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": DEFAULT_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "coderex", "version": "0.1.0"}
            })),
        };
        let result = self
            .send_legacy_once(&request)
            .await
            .map_err(|error| format!("MCP SSE re-initialize failed: {}", error.into_message()))?;
        self.context.capture_protocol_version(&result);

        self.context
            .send_notification(&JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: "initialized".to_string(),
                params: None,
            })
            .await
    }

    /// POST one request to the announced message endpoint and wait for the
    /// matching response on the event stream.
    ///
    /// The waiter is registered *before* the POST, so a server that answers
    /// quickly cannot race the registration. The response arrives on the
    /// long-lived GET stream and is matched by JSON-RPC id.
    async fn send_legacy_once(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<Value, LegacyRequestFailure> {
        // A stream that is gone cannot announce an endpoint; say so instead of
        // waiting out the request timeout.
        match self.event_stream_state() {
            EventStreamState::Unsupported => {
                return Err(LegacyRequestFailure::Transport(format!(
                    "the MCP server at {} does not offer a classic SSE event stream",
                    self.context.url
                )));
            }
            EventStreamState::Degraded => {
                return Err(LegacyRequestFailure::Transport(format!(
                    "the MCP SSE event stream at {} is degraded",
                    self.context.url
                )));
            }
            EventStreamState::Stopped => {
                return Err(LegacyRequestFailure::Transport(format!(
                    "the MCP SSE event stream at {} is not running",
                    self.context.url
                )));
            }
            EventStreamState::Starting | EventStreamState::Active => {}
        }

        // Waiting here is what makes the first request after startup work: the
        // `endpoint` event names the URL the POST must go to.
        self.context
            .wait_for_message_url()
            .await
            .map_err(LegacyRequestFailure::Transport)?;

        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(request.id, sender);

        let response = match self
            .context
            .post(request, "application/json, text/event-stream")
            .await
        {
            Ok(response) => response,
            Err(error) => {
                self.take_pending(request.id);
                return Err(LegacyRequestFailure::Transport(error));
            }
        };

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            self.take_pending(request.id);
            return Err(LegacyRequestFailure::Status { status, body });
        }

        match tokio::time::timeout(LEGACY_SSE_REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(LegacyRequestFailure::Response(error)),
            Ok(Err(_)) => Err(LegacyRequestFailure::Transport(
                "the MCP SSE stream ended before the response arrived".to_string(),
            )),
            Err(_) => {
                self.take_pending(request.id);
                Err(LegacyRequestFailure::Transport(format!(
                    "timed out after {}s waiting for the response to '{}' on the MCP SSE stream",
                    LEGACY_SSE_REQUEST_TIMEOUT.as_secs(),
                    request.method
                )))
            }
        }
    }

    fn take_pending(&self, id: u64) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    pub async fn send_notification(
        &self,
        notification: &JsonRpcNotification,
    ) -> Result<(), String> {
        self.context.send_notification(notification).await
    }

    /// The standalone stream's current state.
    #[must_use]
    pub fn event_stream_state(&self) -> EventStreamState {
        self.event_state
            .read()
            .map_or(EventStreamState::Stopped, |state| *state)
    }

    /// Start the standalone GET stream, replacing any previous one.
    ///
    /// One task per remote server. It shares the handler, session id, and
    /// token provider with the POST path, so a request the server pushes is
    /// answered exactly as one carried by a response stream is.
    pub fn start_event_stream(&self) {
        self.stop_event_stream_now();
        let cancel = CancellationToken::new();
        set_stream_state(&self.event_state, EventStreamState::Starting);
        let context = self.context.clone();
        let handler = Arc::clone(&self.event_handler);
        let notifications = Arc::clone(&self.notification_handler);
        let state = Arc::clone(&self.event_state);
        let policy = self.retry_policy;
        let probe_interval = self.stream_probe_interval;
        let token = cancel.clone();
        let task_state = Arc::clone(&state);
        let mode = self.mode;
        let pending = Arc::clone(&self.pending);
        let task = tokio::spawn(async move {
            let final_state = match mode {
                RemoteMode::StreamableHttp => {
                    run_event_stream(
                        context,
                        handler,
                        notifications,
                        policy,
                        probe_interval,
                        token,
                        task_state,
                    )
                    .await
                }
                RemoteMode::LegacySse => {
                    run_legacy_event_stream(
                        context,
                        handler,
                        notifications,
                        pending,
                        policy,
                        probe_interval,
                        token,
                        task_state,
                    )
                    .await
                }
            };
            set_stream_state(&state, final_state);
        });
        if let Ok(mut slot) = self.event_stream.lock() {
            *slot = Some(EventStreamHandle { cancel, task });
        }
    }

    /// Cancel the GET stream and await the task.
    ///
    /// Cancellation is observed at connection and chunk boundaries; the
    /// timeout bounds the wait if the task is inside a user prompt, which is
    /// not itself cancellable.
    pub async fn stop_event_stream(&self) {
        let handle = self
            .event_stream
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            handle.cancel.cancel();
            if tokio::time::timeout(Duration::from_secs(2), handle.task)
                .await
                .is_err()
            {
                tracing::debug!("the MCP GET stream did not stop in time; aborting it");
            }
        }
        set_stream_state(&self.event_state, EventStreamState::Stopped);
    }

    /// Cancel the stream without awaiting it (used by `Drop` and by restart).
    fn stop_event_stream_now(&self) {
        if let Ok(mut slot) = self.event_stream.lock() {
            if let Some(handle) = slot.take() {
                handle.cancel.cancel();
            }
        }
    }

    /// Whether the GET stream is currently connecting or connected.
    #[must_use]
    pub fn event_stream_active(&self) -> bool {
        matches!(
            self.event_stream_state(),
            EventStreamState::Starting | EventStreamState::Active
        )
    }

    /// Wait for a legacy-SSE stream to connect, failing loudly when the
    /// endpoint does not speak SSE.
    ///
    /// Without this the first request would wait for an `endpoint` event that
    /// a streamable or plain HTTP endpoint will never send. A 405 is reported
    /// as a configuration error instead of being silently retried as if it
    /// were the optional standalone stream of the streamable transport.
    pub async fn await_legacy_stream(&self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match self.event_stream_state() {
                // Connected *and* the endpoint event has been processed: only
                // then can the first POST be sent.
                EventStreamState::Active if self.context.message_url.borrow().is_some() => {
                    return Ok(());
                }
                EventStreamState::Active | EventStreamState::Starting => {}
                EventStreamState::Unsupported => {
                    return Err(format!(
                        "the MCP server at {} answered 405 to the SSE GET; `type: \"sse\"` requires a classic SSE endpoint",
                        self.context.url
                    ));
                }
                EventStreamState::Degraded => {
                    return Err(format!(
                        "the MCP SSE event stream at {} could not be established",
                        self.context.url
                    ));
                }
                EventStreamState::Stopped => {
                    return Err(format!(
                        "the MCP SSE event stream at {} stopped before connecting",
                        self.context.url
                    ));
                }
            }

            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out connecting to the MCP SSE event stream at {}",
                    self.context.url
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for HttpChannel {
    fn drop(&mut self) {
        // A task cannot be awaited here, but it can be told to stop. Its
        // chunk read selects on the token, so the task exits at the next
        // boundary and drops the response body.
        self.stop_event_stream_now();
    }
}

/// The `Bearer ... error="invalid_token"` challenge on a 401, if this is one.
///
/// Only this challenge means "the access token is stale". Any other 401 —
/// `insufficient_scope`, a basic-auth challenge, or no challenge at all — is
/// left to the ordinary error path.
fn invalid_token_challenge(response: &reqwest::Response) -> Option<String> {
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return None;
    }
    let header = response.headers().get(WWW_AUTHENTICATE)?.to_str().ok()?;
    let challenge = crate::oauth::parse_www_authenticate(header);
    (challenge.error.as_deref() == Some("invalid_token")).then(|| header.to_string())
}

/// How one attempt at the standalone stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamEnd {
    /// The server answered 405: it has no standalone stream. Stop quietly.
    Unsupported,
    /// The server answered 404: the session expired; re-initialize once.
    SessionExpired,
    /// The inbound queue overflowed. Stop rather than buffer without bound.
    Overflow,
    /// The task was cancelled by shutdown.
    Cancelled,
    /// The stream closed normally; reconnect.
    Closed,
}

/// The outcome of one connection attempt.
#[derive(Debug)]
struct StreamAttempt {
    end: StreamEnd,
    /// Messages processed, used to reset the retry budget after progress.
    events: usize,
}

/// Make the negotiated protocol revision visible to the stream task, and
/// re-run the initialize handshake after a 404.
async fn reinitialize(
    context: &RequestContext,
    handler: Option<&dyn InboundRequestHandler>,
) -> Result<(), String> {
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        // The request id space belongs to the channel; id 0 is never used for
        // an application request, so it cannot collide with one.
        id: 0,
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": DEFAULT_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "coderex", "version": "0.1.0"}
        })),
    };
    let response = context
        .post(&request, "application/json, text/event-stream")
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "MCP HTTP re-initialize failed with {status}: {body}"
        ));
    }

    // The old session's dedup keys are meaningless now. The new session id is
    // captured by `post`; the protocol revision from the result.
    let notifications: Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>> =
        Arc::new(RwLock::new(None));
    let result = finish_response(context, &notifications, response, request.id, handler).await?;
    context.capture_protocol_version(&result);

    context
        .send_notification(&JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "initialized".to_string(),
            params: None,
        })
        .await
}

fn set_stream_state(state: &Arc<RwLock<EventStreamState>>, value: EventStreamState) {
    if let Ok(mut slot) = state.write() {
        *slot = value;
    }
}

/// What a standalone stream does after it enters `Degraded`.
enum ProbeOutcome {
    /// The probe wait elapsed: try one reconnect, then degrade again on a
    /// failure.
    Rearm,
    /// No probe is configured: leave the stream `Degraded`.
    Stop,
    /// Shutdown cancelled the probe wait.
    Cancelled,
}

/// Enter `Degraded` and wait one probe interval before a single reconnect.
///
/// The interval is the rate limit. A server that stays down is probed once per
/// interval rather than in a loop, so this never busy-spins. With the probe
/// disabled the state is entered only by the caller's `Stop` result.
async fn degrade_and_rearm(
    state: &Arc<RwLock<EventStreamState>>,
    probe_interval: Option<Duration>,
    cancel: &CancellationToken,
) -> ProbeOutcome {
    let Some(interval) = probe_interval else {
        return ProbeOutcome::Stop;
    };

    // Make the degradation observable for the whole wait, not just an instant:
    // a reader (the session, `/mcp`, a test) can see that the stream is down.
    set_stream_state(state, EventStreamState::Degraded);
    tokio::select! {
        () = cancel.cancelled() => ProbeOutcome::Cancelled,
        () = tokio::time::sleep(interval) => ProbeOutcome::Rearm,
    }
}

/// Keep the standalone GET stream open, reconnecting with bounded backoff.
///
/// `Last-Event-ID` resumes from the last event that was processed. A 405 is
/// not an error — it is how a server says the optional stream is unavailable —
/// so it stops the task instead of retrying. A 404 means the session expired:
/// the id is cleared, the handshake runs once more, and the stream reconnects.
/// Exhausting the retry budget enters `Degraded`. With a probe interval
/// configured (see [`MCP_STREAM_PROBE_ENV`]) the task then waits that interval
/// and makes one reconnect, repeating slowly; an interval of `0` leaves the
/// stream `Degraded` until it is restarted.
async fn run_event_stream(
    context: RequestContext,
    handler: Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
    notifications: Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    policy: EventStreamRetryPolicy,
    probe_interval: Option<Duration>,
    cancel: CancellationToken,
    state: Arc<RwLock<EventStreamState>>,
) -> EventStreamState {
    let last_event_id = Arc::new(Mutex::new(None::<String>));
    let mut failures = 0_usize;
    let mut last_progress = Instant::now();
    let mut reinitialized = false;

    loop {
        if cancel.is_cancelled() {
            return EventStreamState::Stopped;
        }
        set_stream_state(&state, EventStreamState::Starting);

        let attempt = read_event_stream_once(
            &context,
            &handler,
            &notifications,
            &last_event_id,
            &cancel,
            &state,
        )
        .await;

        match attempt {
            Err(error) => {
                tracing::warn!(url = %context.url, "MCP GET stream failed: {error}");
            }
            Ok(attempt) => match attempt.end {
                StreamEnd::Cancelled => return EventStreamState::Stopped,
                StreamEnd::Unsupported => {
                    tracing::debug!(
                        url = %context.url,
                        "MCP server does not support a standalone GET stream (405); continuing without it"
                    );
                    return EventStreamState::Unsupported;
                }
                StreamEnd::Overflow => {
                    tracing::warn!(
                        url = %context.url,
                        "the MCP GET stream's inbound queue overflowed; closing it rather than buffering without bound"
                    );
                    match degrade_and_rearm(&state, probe_interval, &cancel).await {
                        ProbeOutcome::Cancelled => return EventStreamState::Stopped,
                        ProbeOutcome::Stop => return EventStreamState::Degraded,
                        ProbeOutcome::Rearm => {
                            failures = policy.max_attempts.saturating_sub(1);
                            last_progress = Instant::now();
                            reinitialized = false;
                            continue;
                        }
                    }
                }
                StreamEnd::SessionExpired => {
                    if reinitialized {
                        tracing::warn!(
                            url = %context.url,
                            "the MCP session expired again after re-initializing; giving up on the GET stream"
                        );
                        match degrade_and_rearm(&state, probe_interval, &cancel).await {
                            ProbeOutcome::Cancelled => return EventStreamState::Stopped,
                            ProbeOutcome::Stop => return EventStreamState::Degraded,
                            ProbeOutcome::Rearm => {
                                failures = policy.max_attempts.saturating_sub(1);
                                last_progress = Instant::now();
                                reinitialized = false;
                                continue;
                            }
                        }
                    }
                    tracing::debug!(
                        url = %context.url,
                        "the MCP session expired; clearing it and re-initializing once"
                    );
                    context.clear_session();
                    let inbound = handler
                        .read()
                        .ok()
                        .and_then(|slot| slot.as_ref().map(Arc::clone));
                    match reinitialize(&context, inbound.as_deref()).await {
                        Ok(()) => {
                            reinitialized = true;
                            failures = 0;
                            last_progress = Instant::now();
                            continue;
                        }
                        Err(error) => {
                            tracing::warn!(
                                url = %context.url,
                                "re-initializing the MCP session failed: {error}"
                            );
                            match degrade_and_rearm(&state, probe_interval, &cancel).await {
                                ProbeOutcome::Cancelled => return EventStreamState::Stopped,
                                ProbeOutcome::Stop => return EventStreamState::Degraded,
                                ProbeOutcome::Rearm => {
                                    failures = policy.max_attempts.saturating_sub(1);
                                    last_progress = Instant::now();
                                    reinitialized = false;
                                    continue;
                                }
                            }
                        }
                    }
                }
                StreamEnd::Closed => {
                    tracing::debug!(
                        url = %context.url,
                        last_event_id = ?last_event_id.lock().ok().and_then(|id| id.clone()),
                        "MCP server's GET stream closed; reconnecting"
                    );
                    if attempt.events > 0 {
                        failures = 0;
                        last_progress = Instant::now();
                        // Progress resets the one-shot session recovery: a
                        // later expiry is a new event, not a loop.
                        reinitialized = false;
                    }
                }
            },
        }

        failures += 1;
        if failures >= policy.max_attempts || last_progress.elapsed() >= policy.max_duration {
            tracing::warn!(
                url = %context.url,
                failures,
                probe_interval = ?probe_interval,
                "the MCP GET stream is degraded"
            );
            match degrade_and_rearm(&state, probe_interval, &cancel).await {
                ProbeOutcome::Cancelled => return EventStreamState::Stopped,
                ProbeOutcome::Stop => return EventStreamState::Degraded,
                ProbeOutcome::Rearm => {
                    // One reconnect per probe: a failure lands on the cap
                    // again instead of spending a full retry budget.
                    failures = policy.max_attempts.saturating_sub(1);
                    last_progress = Instant::now();
                    reinitialized = false;
                    continue;
                }
            }
        }

        let delay = jittered_delay(backoff_delay(&policy, failures));
        tokio::select! {
            () = cancel.cancelled() => return EventStreamState::Stopped,
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// Keep a classic MCP SSE event stream open, reconnecting with bounded
/// backoff.
///
/// Unlike the streamable transport's optional standalone stream, this GET
/// stream *is* the transport: it announces the POST message endpoint and
/// carries every response. A 405 means the configured endpoint does not speak
/// SSE, so the stream stops permanently and requests fail loudly instead of
/// silently behaving like streamable HTTP.
async fn run_legacy_event_stream(
    context: RequestContext,
    handler: Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
    notifications: Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    pending: PendingRequests,
    policy: EventStreamRetryPolicy,
    probe_interval: Option<Duration>,
    cancel: CancellationToken,
    state: Arc<RwLock<EventStreamState>>,
) -> EventStreamState {
    let mut failures = 0_usize;
    let mut last_progress = Instant::now();

    loop {
        if cancel.is_cancelled() {
            return EventStreamState::Stopped;
        }
        set_stream_state(&state, EventStreamState::Starting);

        let attempt = read_legacy_event_stream_once(
            &context,
            &handler,
            &notifications,
            &pending,
            &cancel,
            &state,
        )
        .await;

        match attempt {
            Err(error) => {
                tracing::warn!(url = %context.url, "MCP SSE stream failed: {error}");
            }
            Ok(attempt) => match attempt.end {
                StreamEnd::Cancelled => return EventStreamState::Stopped,
                StreamEnd::Unsupported => {
                    tracing::warn!(
                        url = %context.url,
                        "the configured MCP endpoint answered 405 to the SSE GET; the legacy SSE transport cannot be used"
                    );
                    return EventStreamState::Unsupported;
                }
                StreamEnd::Overflow => {
                    tracing::warn!(
                        url = %context.url,
                        "the MCP SSE inbound queue overflowed; closing it rather than buffering without bound"
                    );
                    match degrade_and_rearm(&state, probe_interval, &cancel).await {
                        ProbeOutcome::Cancelled => return EventStreamState::Stopped,
                        ProbeOutcome::Stop => return EventStreamState::Degraded,
                        ProbeOutcome::Rearm => {
                            failures = policy.max_attempts.saturating_sub(1);
                            last_progress = Instant::now();
                            continue;
                        }
                    }
                }
                StreamEnd::SessionExpired => {
                    // The legacy reader does not use this: a 404 to the GET is
                    // an ordinary failed attempt, bounded by the retry policy.
                }
                StreamEnd::Closed => {
                    tracing::debug!(
                        url = %context.url,
                        "MCP SSE stream closed; reconnecting"
                    );
                    if attempt.events > 0 {
                        failures = 0;
                        last_progress = Instant::now();
                    }
                }
            },
        }

        failures += 1;
        if failures >= policy.max_attempts || last_progress.elapsed() >= policy.max_duration {
            tracing::warn!(
                url = %context.url,
                failures,
                probe_interval = ?probe_interval,
                "the MCP SSE stream is degraded"
            );
            match degrade_and_rearm(&state, probe_interval, &cancel).await {
                ProbeOutcome::Cancelled => return EventStreamState::Stopped,
                ProbeOutcome::Stop => return EventStreamState::Degraded,
                ProbeOutcome::Rearm => {
                    // One reconnect per probe: a failure lands on the cap
                    // again instead of spending a full retry budget.
                    failures = policy.max_attempts.saturating_sub(1);
                    last_progress = Instant::now();
                    continue;
                }
            }
        }

        let delay = jittered_delay(backoff_delay(&policy, failures));
        tokio::select! {
            () = cancel.cancelled() => return EventStreamState::Stopped,
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// Read one connection to the classic SSE stream until it ends.
async fn read_legacy_event_stream_once(
    context: &RequestContext,
    handler: &Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
    notifications: &Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    pending: &PendingRequests,
    cancel: &CancellationToken,
    state: &Arc<RwLock<EventStreamState>>,
) -> Result<StreamAttempt, String> {
    // The endpoint event names the current session's message URL. Forget the
    // previous one so a request cannot POST to a session that is gone.
    context.clear_message_url();

    let accept = HeaderValue::from_static(EVENT_STREAM_ACCEPT);
    let response = tokio::select! {
        () = cancel.cancelled() => {
            return Ok(StreamAttempt { end: StreamEnd::Cancelled, events: 0 });
        }
        response = context.send_with_recovery(
            "connecting to the MCP SSE stream failed",
            |mut headers| {
                headers.insert(ACCEPT, accept.clone());
                context.client.get(context.url.clone()).headers(headers)
            },
        ) => response?,
    };

    let status = response.status();
    if status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
        return Ok(StreamAttempt {
            end: StreamEnd::Unsupported,
            events: 0,
        });
    }
    if !status.is_success() {
        return Err(format!("the MCP SSE GET stream returned {status}"));
    }
    set_stream_state(state, EventStreamState::Active);

    // `(event type, data)` pairs; the worker learns the endpoint URL and
    // routes messages without blocking the socket read while a person answers
    // an elicitation.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, String)>(INBOUND_QUEUE_CAPACITY);
    let worker_context = context.clone();
    let worker_handler = Arc::clone(handler);
    let worker_notifications = Arc::clone(notifications);
    let worker_pending = Arc::clone(pending);
    let worker = tokio::spawn(async move {
        let mut processed = 0_usize;
        while let Some((event_type, data)) = rx.recv().await {
            if process_legacy_event(
                &worker_context,
                &worker_handler,
                &worker_notifications,
                &worker_pending,
                &event_type,
                &data,
            )
            .await
            {
                processed += 1;
            }
        }
        processed
    });

    let mut response = response;
    let mut buffer = Vec::new();
    let mut overflow = false;

    'read: loop {
        let events = take_events(&mut buffer);
        for event in events {
            let Some(data) = sse_event_data(&event) else {
                continue;
            };
            let event_type = sse_event_type(&event).unwrap_or_default();
            if tx.try_send((event_type, data)).is_err() {
                overflow = true;
                break 'read;
            }
        }

        match tokio::select! {
            () = cancel.cancelled() => {
                drop(tx);
                worker.abort();
                return Ok(StreamAttempt { end: StreamEnd::Cancelled, events: 0 });
            }
            chunk = response.chunk() => chunk,
        } {
            Ok(Some(chunk)) => {
                buffer.extend_from_slice(&chunk);
                if buffer.len() > MAX_SSE_EVENT_BYTES {
                    tracing::warn!(
                        url = %context.url,
                        "an MCP SSE event exceeded the size limit; closing the stream"
                    );
                    overflow = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                drop(tx);
                worker.abort();
                return Err(format!("reading the MCP SSE stream failed: {error}"));
            }
        }
    }

    // The final event may not have its terminating blank line before the
    // connection closed.
    if !overflow && !buffer.is_empty() {
        let event = String::from_utf8_lossy(&buffer).into_owned();
        if let Some(data) = sse_event_data(&event) {
            let event_type = sse_event_type(&event).unwrap_or_default();
            if tx.try_send((event_type, data)).is_err() {
                overflow = true;
            }
        }
    }
    drop(tx);

    if overflow {
        worker.abort();
        return Ok(StreamAttempt {
            end: StreamEnd::Overflow,
            events: 0,
        });
    }

    let events = worker.await.unwrap_or(0);
    Ok(StreamAttempt {
        end: StreamEnd::Closed,
        events,
    })
}

/// Route one legacy-SSE event: learn the message endpoint, or dispatch the
/// JSON-RPC message it carries.
async fn process_legacy_event(
    context: &RequestContext,
    handler: &Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
    notifications: &Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    pending: &PendingRequests,
    event_type: &str,
    data: &str,
) -> bool {
    if event_type == "endpoint" {
        let endpoint = data.trim();
        if endpoint.is_empty() {
            return false;
        }
        match context.url.join(endpoint) {
            Ok(url) => {
                tracing::debug!(endpoint = %url, "MCP SSE server announced its message endpoint");
                context.set_message_url(url);
                true
            }
            Err(error) => {
                tracing::warn!("the MCP SSE endpoint event named an invalid URL: {error}");
                false
            }
        }
    } else if event_type.is_empty() || event_type == "message" {
        process_legacy_message(context, handler, notifications, pending, data).await
    } else {
        // Unknown event types are allowed by SSE; ignore them rather than
        // failing a stream that may still carry the messages we need.
        tracing::debug!(event_type, "ignoring an unknown MCP SSE event type");
        false
    }
}

/// Route one JSON-RPC message delivered over the classic SSE stream.
async fn process_legacy_message(
    context: &RequestContext,
    handler: &Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
    notifications: &Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    pending: &PendingRequests,
    data: &str,
) -> bool {
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return false;
    }

    // Keep-alives and other non-JSON payloads are skipped.
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return false;
    };

    let has_method = value.get("method").is_some();
    let has_id = value.get("id").is_some();

    match (has_method, has_id) {
        // A request from the server, answered over the message endpoint.
        (true, true) => match serde_json::from_value::<JsonRpcServerRequest>(value.clone()) {
            Ok(request) => {
                let handler = handler
                    .read()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(Arc::clone));
                if let Err(error) = context.answer_once(&request, handler.as_deref()).await {
                    tracing::warn!(
                        method = %request.method,
                        "could not answer a request from the MCP SSE stream: {error}"
                    );
                }
            }
            Err(error) => {
                tracing::warn!("failed to parse a server request from the MCP SSE stream: {error}");
            }
        },
        // A notification, which may invalidate discovery or cached resources.
        (true, false) => {
            let method = value.get("method").and_then(Value::as_str).unwrap_or("");
            dispatch_notification(notifications, method, value.get("params")).await;
        }
        // A response: hand it to the request waiting on that id.
        (false, true) => {
            let response: JsonRpcResponse = match serde_json::from_value(value.clone()) {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!("failed to parse a response from the MCP SSE stream: {error}");
                    return false;
                }
            };
            let waiter = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&response.id);
            if let Some(sender) = waiter {
                let outcome = match response.error {
                    Some(error) => Err(format!(
                        "JSON-RPC error (code {}): {}",
                        error.code, error.message
                    )),
                    None => Ok(response.result.unwrap_or(Value::Null)),
                };
                let _ = sender.send(outcome);
            } else {
                tracing::debug!(
                    id = response.id,
                    "a response arrived on the MCP SSE stream with no request waiting on it"
                );
            }
        }
        (false, false) => {
            tracing::warn!(
                message = %value,
                "a message on the MCP SSE stream is neither a response nor a request"
            );
        }
    }

    true
}

/// The exponential part of the retry delay, capped by the policy.
fn backoff_delay(policy: &EventStreamRetryPolicy, failures: usize) -> Duration {
    // Capped at 16 above, so the conversion cannot truncate anything.
    let shift = u32::try_from(failures.saturating_sub(1).min(16)).unwrap_or(16);
    policy
        .base_delay
        .saturating_mul(1_u32 << shift)
        .min(policy.max_delay)
}

/// Add up to 25% jitter so reconnecting clients do not synchronize.
fn jittered_delay(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| u64::from(elapsed.subsec_nanos()));
    // A duration too long for `u64` milliseconds is unreachable in practice;
    // saturate instead of truncating.
    let jitter = (u64::try_from(base.as_millis()).unwrap_or(u64::MAX) / 4).max(1);
    base + Duration::from_millis(nanos % jitter)
}

/// Read one connection to the standalone stream until it ends.
async fn read_event_stream_once(
    context: &RequestContext,
    handler: &Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
    notifications: &Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    last_event_id: &Arc<Mutex<Option<String>>>,
    cancel: &CancellationToken,
    state: &Arc<RwLock<EventStreamState>>,
) -> Result<StreamAttempt, String> {
    let accept = HeaderValue::from_static(EVENT_STREAM_ACCEPT);
    let resume_from = last_event_id.lock().ok().and_then(|slot| slot.clone());
    let resume_value = resume_from
        .as_deref()
        .and_then(|id| match HeaderValue::from_str(id) {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!("MCP Last-Event-ID cannot be sent as an HTTP header: {error}");
                None
            }
        });

    let response = tokio::select! {
        () = cancel.cancelled() => {
            return Ok(StreamAttempt { end: StreamEnd::Cancelled, events: 0 });
        }
        response = context.send_with_recovery(
            "connecting to the GET stream failed",
            |mut headers| {
                headers.insert(ACCEPT, accept.clone());
                if let Some(value) = &resume_value {
                    headers.insert(
                        HeaderName::from_static(LAST_EVENT_ID_HEADER),
                        value.clone(),
                    );
                }
                context.client.get(context.url.clone()).headers(headers)
            },
        ) => response?,
    };

    let status = response.status();
    if status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
        return Ok(StreamAttempt {
            end: StreamEnd::Unsupported,
            events: 0,
        });
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(StreamAttempt {
            end: StreamEnd::SessionExpired,
            events: 0,
        });
    }
    if !status.is_success() {
        return Err(format!("the GET stream returned {status}"));
    }
    set_stream_state(state, EventStreamState::Active);

    // A bounded inbound queue: a server that pushes messages faster than the
    // handler can process them makes the stream close (and degrade) instead of
    // growing an in-memory buffer without limit.
    let (tx, mut rx) =
        tokio::sync::mpsc::channel::<(String, Option<String>)>(INBOUND_QUEUE_CAPACITY);
    let worker_context = context.clone();
    let worker_handler = Arc::clone(handler);
    let worker_notifications = Arc::clone(notifications);
    let worker_last_id = Arc::clone(last_event_id);
    let worker = tokio::spawn(async move {
        let mut processed = 0_usize;
        while let Some((event, event_id)) = rx.recv().await {
            let handled = process_stream_event(
                &worker_context,
                &worker_handler,
                &worker_notifications,
                &event,
            )
            .await;
            if handled {
                processed += 1;
            }
            if let Some(id) = event_id {
                if let Ok(mut slot) = worker_last_id.lock() {
                    *slot = Some(id);
                }
            }
        }
        processed
    });

    let mut response = response;
    let mut buffer = Vec::new();
    let mut overflow = false;

    'read: loop {
        let events = take_events(&mut buffer);
        for event in events {
            let event_id = sse_event_id(&event);
            if tx.try_send((event, event_id)).is_err() {
                overflow = true;
                break 'read;
            }
        }

        match tokio::select! {
            () = cancel.cancelled() => {
                drop(tx);
                worker.abort();
                return Ok(StreamAttempt { end: StreamEnd::Cancelled, events: 0 });
            }
            chunk = response.chunk() => chunk,
        } {
            Ok(Some(chunk)) => {
                buffer.extend_from_slice(&chunk);
                if buffer.len() > MAX_SSE_EVENT_BYTES {
                    tracing::warn!(
                        url = %context.url,
                        "an MCP SSE event exceeded the size limit; closing the stream"
                    );
                    overflow = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                drop(tx);
                worker.abort();
                return Err(format!("reading the GET stream failed: {error}"));
            }
        }
    }

    // The final event may not have its terminating blank line before the
    // connection closed.
    if !overflow && !buffer.is_empty() {
        let event = String::from_utf8_lossy(&buffer).into_owned();
        let event_id = sse_event_id(&event);
        if tx.try_send((event, event_id)).is_err() {
            overflow = true;
        }
    }
    drop(tx);

    if overflow {
        worker.abort();
        return Ok(StreamAttempt {
            end: StreamEnd::Overflow,
            events: 0,
        });
    }

    let events = worker.await.unwrap_or(0);
    Ok(StreamAttempt {
        end: StreamEnd::Closed,
        events,
    })
}

/// Route one event from the standalone stream.
///
/// Server requests are answered over a POST (with the session id attached by
/// [`RequestContext`]); notifications are logged. Responses have no requester
/// on this stream, so they are only noted.
async fn process_stream_event(
    context: &RequestContext,
    handler: &Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
    notifications: &Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    event: &str,
) -> bool {
    let Some(data) = sse_event_data(event) else {
        return false;
    };
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return false;
    }

    // Keep-alives and other non-JSON payloads are skipped, as on a response
    // stream.
    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        return false;
    };

    let has_method = value.get("method").is_some();
    let has_id = value.get("id").is_some();

    match (has_method, has_id) {
        (true, true) => match serde_json::from_value::<JsonRpcServerRequest>(value.clone()) {
            Ok(request) => {
                // Clone the handler out of the lock before awaiting: the
                // handler prompts a person, and holding the lock across that
                // would block a later install.
                let handler = handler
                    .read()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(Arc::clone));
                if let Err(error) = context.answer_once(&request, handler.as_deref()).await {
                    tracing::warn!(
                        method = %request.method,
                        "could not answer a request from the GET stream: {error}"
                    );
                }
            }
            Err(error) => {
                tracing::warn!("failed to parse a server request from the GET stream: {error}");
            }
        },
        // A notification: no answer is expected, but it can trigger a
        // re-discovery or invalidate a cached resource.
        (true, false) => {
            let method = value.get("method").and_then(Value::as_str).unwrap_or("");
            dispatch_notification(notifications, method, value.get("params")).await;
        }
        // A response with no request waiting on this stream.
        (false, true) => {
            tracing::debug!(
                message = %value,
                "a response arrived on the GET stream with no request waiting on it"
            );
        }
        (false, false) => {
            tracing::warn!(
                message = %value,
                "a message on the GET stream is neither a response nor a request"
            );
        }
    }

    // The caller records the resume point only after this returns, so a
    // reconnect cannot skip a request that was not answered.
    true
}

/// Hand a notification to the session's sink, or log it when there is none.
async fn dispatch_notification(
    notifications: &Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    method: &str,
    params: Option<&Value>,
) {
    let handler = notifications
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().map(Arc::clone));
    if let Some(handler) = handler {
        handler.handle_notification(method, params).await;
    } else {
        tracing::debug!(
            method,
            "notification from the server (no notification handler installed)"
        );
    }
}

/// Read the response to one request, which may be JSON or SSE.
async fn finish_response(
    context: &RequestContext,
    notifications: &Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    response: reqwest::Response,
    expected_id: u64,
    handler: Option<&dyn InboundRequestHandler>,
) -> Result<Value, String> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    if content_type.contains("text/event-stream") {
        return read_sse_response(context, notifications, response, expected_id, handler).await;
    }

    let body = response
        .text()
        .await
        .map_err(|error| format!("failed to read MCP HTTP response: {error}"))?;
    if body.trim().is_empty() {
        return Err(format!(
            "MCP HTTP server returned an empty response to request {expected_id}"
        ));
    }

    let value: Value = serde_json::from_str(&body)
        .map_err(|error| format!("failed to parse MCP HTTP response: {error}"))?;
    route_message(context, notifications, &value, expected_id, handler)
        .await?
        .ok_or_else(|| {
            format!("MCP HTTP response to request {expected_id} contained no response to it")
        })
}

/// Read an SSE response stream, answering server requests as they arrive.
///
/// Read incrementally rather than buffering the whole body: a server that
/// asks a question mid-stream may hold its own response until the answer
/// arrives, so waiting for the end of the body would deadlock.
async fn read_sse_response(
    context: &RequestContext,
    notifications: &Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    mut response: reqwest::Response,
    expected_id: u64,
    handler: Option<&dyn InboundRequestHandler>,
) -> Result<Value, String> {
    // Bytes, not `String`: a network chunk may split a multi-byte UTF-8
    // character, and decoding each chunk on its own would replace half of
    // it with U+FFFD. Event boundaries are ASCII, so a complete event is
    // always valid UTF-8 and the partial tail can stay undecoded.
    let mut buffer = Vec::new();

    loop {
        for event in take_events(&mut buffer) {
            if let Some(result) =
                route_sse_event(context, notifications, &event, expected_id, handler).await?
            {
                return Ok(result);
            }
        }

        match response.chunk().await {
            Ok(Some(chunk)) => {
                buffer.extend_from_slice(&chunk);
                if buffer.len() > MAX_SSE_EVENT_BYTES {
                    return Err("an MCP SSE response event exceeded the size limit".to_string());
                }
            }
            Ok(None) => break,
            Err(error) => return Err(format!("failed to read MCP SSE response: {error}")),
        }
    }

    // A stream may end without the blank line that terminates its last
    // event; the data that arrived is still a message.
    if !buffer.is_empty() {
        let event = String::from_utf8_lossy(&buffer).into_owned();
        buffer.clear();
        if let Some(result) =
            route_sse_event(context, notifications, &event, expected_id, handler).await?
        {
            return Ok(result);
        }
    }

    Err(format!(
        "SSE stream ended before a response to request {expected_id}"
    ))
}

/// Parse one SSE event and route the JSON-RPC message it carries.
async fn route_sse_event(
    context: &RequestContext,
    notifications: &Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    event: &str,
    expected_id: u64,
    handler: Option<&dyn InboundRequestHandler>,
) -> Result<Option<Value>, String> {
    let Some(data) = sse_event_data(event) else {
        return Ok(None);
    };
    if data.trim() == "[DONE]" {
        return Ok(None);
    }

    // Not every SSE `data:` field is a JSON-RPC message (servers may send
    // keep-alives); skip what does not parse rather than failing the call.
    let value: Value = match serde_json::from_str(&data) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    route_message(context, notifications, &value, expected_id, handler).await
}

/// Tell a response, a notification, and a server request apart by shape.
///
/// Returns `Some(result)` only when the response to `expected_id` arrives.
async fn route_message(
    context: &RequestContext,
    notifications: &Arc<RwLock<Option<Arc<dyn ServerNotificationHandler>>>>,
    value: &Value,
    expected_id: u64,
    handler: Option<&dyn InboundRequestHandler>,
) -> Result<Option<Value>, String> {
    let has_method = value.get("method").is_some();
    let has_id = value.get("id").is_some();

    match (has_method, has_id) {
        // The server asking us to do something, and waiting for the answer.
        (true, true) => {
            let request: JsonRpcServerRequest = serde_json::from_value(value.clone())
                .map_err(|error| format!("failed to parse a server request: {error}"))?;
            context.answer_once(&request, handler).await?;
            Ok(None)
        }
        // A notification: no answer is expected, but it may ask us to refresh
        // discovery or invalidate a cached resource.
        (true, false) => {
            let method = value.get("method").and_then(Value::as_str).unwrap_or("");
            dispatch_notification(notifications, method, value.get("params")).await;
            Ok(None)
        }
        (false, true) => {
            let response: JsonRpcResponse = serde_json::from_value(value.clone())
                .map_err(|error| format!("failed to parse JSON-RPC response: {error}"))?;

            if response.id != expected_id {
                // An answer to a request that is no longer being waited on.
                tracing::debug!(
                    id = response.id,
                    expected = expected_id,
                    "a response arrived for a request nothing is waiting on"
                );
                return Ok(None);
            }
            if let Some(error) = response.error {
                return Err(format!(
                    "JSON-RPC error (code {}): {}",
                    error.code, error.message
                ));
            }
            Ok(Some(response.result.unwrap_or(Value::Null)))
        }
        // Neither shape: say so rather than read past it.
        (false, false) => {
            tracing::warn!(
                message = %value,
                "a message from the server is neither a response nor a request"
            );
            Ok(None)
        }
    }
}

/// Drain every complete SSE event from `buffer`, leaving any partial one.
///
/// An event ends at a blank line; the three line-ending spellings the SSE
/// grammar allows are all accepted. Separators are found on the raw bytes, so
/// one split across two chunks is still recognised once the rest arrives.
fn take_events(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut events = Vec::new();
    while let Some((index, separator_len)) = find_event_boundary(buffer.as_slice()) {
        events.push(String::from_utf8_lossy(&buffer[..index]).into_owned());
        buffer.drain(..index + separator_len);
    }
    events
}

/// Find the earliest `\n\n`, `\r\n\r\n`, or `\r\r` in `bytes`.
fn find_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if bytes[index] == b'\n' && bytes[index..].starts_with(b"\n\n") {
            return Some((index, 2));
        }
        if bytes[index] == b'\r' && bytes[index..].starts_with(b"\r\r") {
            return Some((index, 2));
        }
        index += 1;
    }
    None
}

/// The `id:` field of one SSE event, if it has one.
///
/// The id is what a reconnect sends back as `Last-Event-ID` so the server can
/// resume after the last message the client actually processed.
fn sse_event_id(event: &str) -> Option<String> {
    for line in event.split(['\n', '\r']) {
        let Some(rest) = line.strip_prefix("id:") else {
            continue;
        };
        let id = rest.strip_prefix(' ').unwrap_or(rest).trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

/// The `event:` field of one SSE event, if it has one.
fn sse_event_type(event: &str) -> Option<String> {
    for line in event.split(['\n', '\r']) {
        let Some(rest) = line.strip_prefix("event:") else {
            continue;
        };
        let event_type = rest.strip_prefix(' ').unwrap_or(rest).trim();
        if !event_type.is_empty() {
            return Some(event_type.to_string());
        }
    }
    None
}

/// The `data:` payload of one SSE event, with multi-line data joined by `\n`.
fn sse_event_data(event: &str) -> Option<String> {
    let mut data = Vec::new();
    for line in event.split(['\n', '\r']) {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        data.push(rest.strip_prefix(' ').unwrap_or(rest));
    }

    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(body: &str) -> Vec<String> {
        let mut buffer = body.as_bytes().to_vec();
        let events = take_events(&mut buffer);
        assert!(buffer.is_empty(), "left a partial event: {buffer:?}");
        events
    }

    #[test]
    fn splits_sse_events_and_keeps_the_data_payload() {
        let body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"tools\":[]}}\n\n",
            "data: [DONE]\n\n"
        );
        let events = events(body);
        assert_eq!(events.len(), 2);
        assert_eq!(
            sse_event_data(&events[0]).as_deref(),
            Some(r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[]}}"#)
        );
        assert_eq!(sse_event_data(&events[1]).as_deref(), Some("[DONE]"));
    }

    #[test]
    fn joins_multiline_data_and_accepts_other_line_endings() {
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\r\n\r\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":9,\r\n",
            "data: \"result\":{\"ok\":true}}\r\r"
        );
        let events = events(body);
        assert_eq!(events.len(), 2);
        assert_eq!(
            sse_event_data(&events[1]).as_deref(),
            Some("{\"jsonrpc\":\"2.0\",\"id\":9,\n\"result\":{\"ok\":true}}")
        );
    }

    #[test]
    fn a_partial_event_stays_in_the_buffer_for_the_next_chunk() {
        let mut buffer = b"data: {\"jsonrpc\":\"2.0\",\"id\":1,".to_vec();
        assert!(take_events(&mut buffer).is_empty());
        buffer.extend_from_slice(b"\"result\":{}}\n\n");
        let events = take_events(&mut buffer);
        assert_eq!(events.len(), 1);
        assert_eq!(
            sse_event_data(&events[0]).as_deref(),
            Some("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}")
        );
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_is_not_corrupted() {
        // "é" is two bytes; split them the way the network can.
        let mut buffer = b"data: {\"text\":\"caf".to_vec();
        buffer.push(0xC3);
        assert!(take_events(&mut buffer).is_empty());
        buffer.extend_from_slice(b"\xA9\"}\n\n");
        let events = take_events(&mut buffer);
        assert_eq!(events.len(), 1);
        assert_eq!(
            sse_event_data(&events[0]).as_deref(),
            Some("{\"text\":\"café\"}")
        );
    }

    #[test]
    fn rejects_invalid_urls_and_headers() {
        assert!(HttpChannel::new("file:///tmp/mcp", HashMap::new()).is_err());
        let headers = HashMap::from([("bad header".into(), "value".into())]);
        assert!(HttpChannel::new("http://localhost/mcp", headers).is_err());
    }
}
