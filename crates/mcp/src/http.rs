//! HTTP and SSE JSON-RPC transport for remote MCP servers.

use crate::channel::{
    InboundRequestHandler, JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    JsonRpcServerRequest, METHOD_NOT_FOUND,
};
use reqwest::Url;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The header the MCP streamable-HTTP transport uses to name a session.
const SESSION_ID_HEADER: &str = "mcp-session-id";
/// The header that resumes an SSE stream where it left off.
const LAST_EVENT_ID_HEADER: &str = "last-event-id";
/// The `Accept` value a standalone server-to-client stream requires.
const EVENT_STREAM_ACCEPT: &str = "text/event-stream";

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
}

impl RequestContext {
    fn session_id(&self) -> Option<String> {
        self.session_id
            .read()
            .ok()
            .and_then(|slot| slot.clone())
    }

    fn clear_session_id(&self) {
        if let Ok(mut slot) = self.session_id.write() {
            *slot = None;
        }
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

    /// Headers for one request, with a freshly resolved bearer token and the
    /// current session id.
    ///
    /// A token from the provider replaces a configured `Authorization` header;
    /// this is what lets a configured static token give way to a refreshed one.
    /// If the provider has nothing, the configured headers go out unchanged.
    async fn request_headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        if let Some(provider) = &self.token_provider {
            if let Some(token) = provider.access_token().await {
                match HeaderValue::from_str(&format!("Bearer {token}")) {
                    Ok(value) => {
                        headers.insert(AUTHORIZATION, value);
                    }
                    Err(error) => {
                        tracing::warn!("MCP OAuth token cannot be sent as an HTTP header: {error}");
                    }
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
        headers
    }

    /// POST one JSON-RPC message, remembering any session id the answer sets.
    async fn post<T: Serialize>(
        &self,
        body: &T,
        accept: &'static str,
    ) -> Result<reqwest::Response, String> {
        let headers = self.request_headers().await;
        let response = self
            .client
            .post(self.url.clone())
            .headers(headers)
            .header(ACCEPT, accept)
            .header(CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await
            .map_err(|error| format!("MCP HTTP request failed: {error}"))?;
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

        let headers = self.request_headers().await;
        let delivered = self
            .client
            .post(self.url.clone())
            .headers(headers)
            .header(CONTENT_TYPE, "application/json")
            .json(&response)
            .send()
            .await
            .map_err(|error| {
                format!(
                    "failed to answer the server's '{}' request: {error}",
                    request.method
                )
            })?;

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

    async fn send_notification(
        &self,
        notification: &JsonRpcNotification,
    ) -> Result<(), String> {
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
    /// The handler shared with the GET-stream task, per event.
    ///
    /// Shared rather than copied so a handler installed after the stream
    /// started (the session's UI slot is one) is seen by later messages.
    event_handler: Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
    /// The one background GET-stream task for this server, if it is running.
    event_stream: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl HttpChannel {
    pub fn new(url: &str, headers: HashMap<String, String>) -> Result<Self, String> {
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

        Ok(Self {
            context: RequestContext {
                client: reqwest::Client::new(),
                url,
                headers: header_map,
                token_provider: None,
                session_id: Arc::new(RwLock::new(None)),
            },
            event_handler: Arc::new(RwLock::new(None)),
            event_stream: std::sync::Mutex::new(None),
        })
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
        let mut reinitialized = false;
        loop {
            let response = self
                .context
                .post(request, "application/json, text/event-stream")
                .await?;
            let status = response.status();

            let session_expired = matches!(status.as_u16(), 404 | 405)
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
                self.context.clear_session_id();
                self.reinitialize(handler).await?;
                reinitialized = true;
                continue;
            }

            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(format!("MCP HTTP server returned {status}: {body}"));
            }

            return self.finish_response(response, request.id, handler).await;
        }
    }

    /// Run the initialize/initialized handshake again on this channel.
    ///
    /// Used only to recover an expired session. The new session id is captured
    /// from the response header like the first one was.
    async fn reinitialize(
        &self,
        handler: Option<&dyn InboundRequestHandler>,
    ) -> Result<(), String> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            // The request id space belongs to the channel; id 0 is never used
            // for an application request, so it cannot collide with one.
            id: 0,
            method: "initialize".to_string(),
            params: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "coderex", "version": "0.1.0"}
            })),
        };
        let response = self
            .context
            .post(&request, "application/json, text/event-stream")
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "MCP HTTP re-initialize failed with {status}: {body}"
            ));
        }
        self.finish_response(response, request.id, handler).await?;

        self.context
            .send_notification(&JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: "initialized".to_string(),
                params: None,
            })
            .await
    }

    /// Read the response to one request, which may be JSON or SSE.
    async fn finish_response(
        &self,
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
            return self.read_sse_response(response, expected_id, handler).await;
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
        self.route_message(&value, expected_id, handler)
            .await?
            .ok_or_else(|| {
                format!(
                    "MCP HTTP response to request {expected_id} contained no response to it"
                )
            })
    }

    /// Read an SSE response stream, answering server requests as they arrive.
    ///
    /// Read incrementally rather than buffering the whole body: a server that
    /// asks a question mid-stream may hold its own response until the answer
    /// arrives, so waiting for the end of the body would deadlock.
    async fn read_sse_response(
        &self,
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
                if let Some(result) = self.route_sse_event(&event, expected_id, handler).await? {
                    return Ok(result);
                }
            }

            match response.chunk().await {
                Ok(Some(chunk)) => buffer.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(error) => return Err(format!("failed to read MCP SSE response: {error}")),
            }
        }

        // A stream may end without the blank line that terminates its last
        // event; the data that arrived is still a message.
        if !buffer.is_empty() {
            let event = String::from_utf8_lossy(&buffer).into_owned();
            buffer.clear();
            if let Some(result) = self.route_sse_event(&event, expected_id, handler).await? {
                return Ok(result);
            }
        }

        Err(format!(
            "SSE stream ended before a response to request {expected_id}"
        ))
    }

    /// Parse one SSE event and route the JSON-RPC message it carries.
    async fn route_sse_event(
        &self,
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
        self.route_message(&value, expected_id, handler).await
    }

    /// Tell a response, a notification, and a server request apart by shape.
    ///
    /// Returns `Some(result)` only when the response to `expected_id` arrives.
    async fn route_message(
        &self,
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
                self.context
                    .answer_server_request(&request, handler)
                    .await?;
                Ok(None)
            }
            // A notification: no answer is expected.
            (true, false) => {
                tracing::debug!(
                    message = %value,
                    "ignoring a notification from the server"
                );
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

    pub async fn send_notification(
        &self,
        notification: &JsonRpcNotification,
    ) -> Result<(), String> {
        self.context.send_notification(notification).await
    }

    /// Start the standalone GET stream, replacing any previous one.
    ///
    /// One task per remote server. It shares the handler, session id, and
    /// token provider with the POST path, so a request the server pushes is
    /// answered exactly as one carried by a response stream is.
    pub fn start_event_stream(&self) {
        self.stop_event_stream();
        let context = self.context.clone();
        let handler = Arc::clone(&self.event_handler);
        let task = tokio::spawn(async move {
            run_event_stream(context, handler).await;
        });
        if let Ok(mut slot) = self.event_stream.lock() {
            *slot = Some(task);
        }
    }

    /// Stop the GET stream, if one is running.
    pub fn stop_event_stream(&self) {
        if let Ok(mut slot) = self.event_stream.lock() {
            if let Some(task) = slot.take() {
                task.abort();
            }
        }
    }

    /// Whether the GET stream task is still running (for diagnostics/tests).
    #[must_use]
    pub fn event_stream_active(&self) -> bool {
        self.event_stream
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|task| !task.is_finished()))
            .unwrap_or(false)
    }
}

impl Drop for HttpChannel {
    fn drop(&mut self) {
        self.stop_event_stream();
    }
}

/// How one attempt at the standalone stream ended.
enum StreamEnd {
    /// The server answered 405: it has no standalone stream. Stop quietly.
    Unsupported,
    /// The stream closed normally; reconnect.
    Closed,
}

/// Keep the standalone GET stream open, reconnecting with bounded backoff.
///
/// `Last-Event-ID` resumes from the last event that was processed. A 405 is
/// not an error — it is how a server says the optional stream is unavailable —
/// so it stops the task instead of retrying.
async fn run_event_stream(
    context: RequestContext,
    handler: Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
) {
    let mut last_event_id: Option<String> = None;
    let mut backoff = Duration::from_millis(250);

    loop {
        match read_event_stream_once(&context, &handler, &mut last_event_id).await {
            Ok(StreamEnd::Unsupported) => {
                tracing::debug!(
                    url = %context.url,
                    "MCP server does not support a standalone GET stream (405); continuing without it"
                );
                return;
            }
            Ok(StreamEnd::Closed) => {
                tracing::debug!(
                    url = %context.url,
                    last_event_id = ?last_event_id,
                    "MCP server's GET stream closed; reconnecting"
                );
            }
            Err(error) => {
                tracing::warn!(url = %context.url, "MCP GET stream failed: {error}");
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

/// Read one connection to the standalone stream until it ends.
async fn read_event_stream_once(
    context: &RequestContext,
    handler: &Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
    last_event_id: &mut Option<String>,
) -> Result<StreamEnd, String> {
    let mut headers = context.request_headers().await;
    headers.insert(ACCEPT, HeaderValue::from_static(EVENT_STREAM_ACCEPT));
    if let Some(id) = last_event_id.as_deref() {
        match HeaderValue::from_str(id) {
            Ok(value) => {
                headers.insert(HeaderName::from_static(LAST_EVENT_ID_HEADER), value);
            }
            Err(error) => {
                tracing::warn!("MCP Last-Event-ID cannot be sent as an HTTP header: {error}");
            }
        }
    }

    let response = context
        .client
        .get(context.url.clone())
        .headers(headers)
        .send()
        .await
        .map_err(|error| format!("connecting to the GET stream failed: {error}"))?;

    if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
        return Ok(StreamEnd::Unsupported);
    }
    if !response.status().is_success() {
        return Err(format!("the GET stream returned {}", response.status()));
    }

    let mut response = response;
    let mut buffer = Vec::new();
    loop {
        for event in take_events(&mut buffer) {
            process_stream_event(context, handler, &event, last_event_id).await;
        }

        match response.chunk().await {
            Ok(Some(chunk)) => buffer.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(error) => return Err(format!("reading the GET stream failed: {error}")),
        }
    }

    // The final event may not have its terminating blank line before the
    // connection closed.
    if !buffer.is_empty() {
        let event = String::from_utf8_lossy(&buffer).into_owned();
        process_stream_event(context, handler, &event, last_event_id).await;
    }

    Ok(StreamEnd::Closed)
}

/// Route one event from the standalone stream.
///
/// Server requests are answered over a POST (with the session id attached by
/// [`RequestContext`]); notifications are logged. Responses have no requester
/// on this stream, so they are only noted.
async fn process_stream_event(
    context: &RequestContext,
    handler: &Arc<RwLock<Option<Arc<dyn InboundRequestHandler>>>>,
    event: &str,
    last_event_id: &mut Option<String>,
) {
    let event_id = sse_event_id(event);

    let Some(data) = sse_event_data(event) else {
        if let Some(id) = event_id {
            *last_event_id = Some(id);
        }
        return;
    };
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        if let Some(id) = event_id {
            *last_event_id = Some(id);
        }
        return;
    }

    // Keep-alives and other non-JSON payloads are skipped, as on a response
    // stream.
    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        if let Some(id) = event_id {
            *last_event_id = Some(id);
        }
        return;
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
                if let Err(error) = context
                    .answer_server_request(&request, handler.as_deref())
                    .await
                {
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
        // A notification: no answer is expected.
        (true, false) => {
            tracing::debug!(
                message = %value,
                "notification from the server's GET stream"
            );
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

    // Record the resume point only after the message has been handled, so a
    // reconnect cannot skip a request that was not answered.
    if let Some(id) = event_id {
        *last_event_id = Some(id);
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
