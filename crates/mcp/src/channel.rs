//! JSON-RPC channel over stdio — subprocess management and message exchange.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// A JSON-RPC request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// JSON-RPC version.
    pub jsonrpc: String,
    /// Unique request ID.
    pub id: u64,
    /// Method name.
    pub method: String,
    /// Parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// JSON-RPC version.
    pub jsonrpc: String,
    /// Matching request ID.
    pub id: u64,
    /// Result (if successful).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error (if failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC's "no such method" error code.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// A JSON-RPC error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Error code.
    pub code: i64,
    /// Error message.
    pub message: String,
    /// Optional extra data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A JSON-RPC request *from* the server, which the client has to answer.
///
/// The protocol runs both ways: the server asks the client to do things —
/// `elicitation/create` is one — and waits for the answer. These used to be
/// parsed as responses and dropped when their id matched nothing the client
/// had sent, so the server waited until its own timeout for an answer that was
/// never coming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcServerRequest {
    /// JSON-RPC version.
    pub jsonrpc: String,
    /// The server's request ID, which the answer must carry back.
    pub id: u64,
    /// Method name.
    pub method: String,
    /// Parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Answers requests the server makes of the client.
///
/// Asynchronous because some of these requests are questions for a person —
/// `elicitation/create` is the common one — and the server waits for the
/// answer. A handler may await the UI; returning an error is how a handler
/// that cannot answer tells the server so rather than leaving it waiting.
#[async_trait::async_trait]
pub trait InboundRequestHandler: Send + Sync + std::fmt::Debug {
    /// Answer one request, or say why it cannot be answered.
    async fn handle(&self, method: &str, params: Option<&Value>) -> Result<Value, JsonRpcError>;
}

/// A handler slot that can be filled (or emptied) while servers keep running.
///
/// The MCP manager installs one of these on every server it creates, and the
/// session fills it once it knows how it can prompt. That ordering matters:
/// servers are spawned during startup, before the interactive UI exists, but
/// the request that needs the UI arrives later. When the slot is empty the
/// handler behaves like an absent one and refuses with `-32601`.
#[derive(Debug, Clone, Default)]
pub struct SharedInboundHandler {
    inner: std::sync::Arc<std::sync::RwLock<Option<std::sync::Arc<dyn InboundRequestHandler>>>>,
}

impl SharedInboundHandler {
    /// An empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Put `handler` in the slot, replacing any previous one.
    pub fn install(&self, handler: std::sync::Arc<dyn InboundRequestHandler>) {
        if let Ok(mut slot) = self.inner.write() {
            *slot = Some(handler);
        }
    }

    /// Empty the slot, so later requests are refused with `-32601`.
    pub fn clear(&self) {
        if let Ok(mut slot) = self.inner.write() {
            *slot = None;
        }
    }

    /// Whether a handler is currently installed.
    #[must_use]
    pub fn has_handler(&self) -> bool {
        self.inner.read().is_ok_and(|slot| slot.is_some())
    }
}

#[async_trait::async_trait]
impl InboundRequestHandler for SharedInboundHandler {
    async fn handle(&self, method: &str, params: Option<&Value>) -> Result<Value, JsonRpcError> {
        // Clone out of the lock before awaiting: the handler may prompt the
        // user, and holding a lock across that would block a later install.
        let handler = self
            .inner
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().map(std::sync::Arc::clone));

        match handler {
            Some(handler) => handler.handle(method, params).await,
            None => Err(JsonRpcError {
                code: METHOD_NOT_FOUND,
                message: format!(
                    "this client has no handler for '{method}'; nothing here can answer it"
                ),
                data: None,
            }),
        }
    }
}

/// Receives notifications the server sends without waiting for an answer.
///
/// Most notifications are informational, but two change what the client
/// knows: `notifications/tools/list_changed` means the tool list must be
/// fetched again, and `notifications/resources/updated` means the content of a
/// cached resource is stale. The manager installs a handler that acts on them
/// rather than only logging them.
#[async_trait::async_trait]
pub trait ServerNotificationHandler: Send + Sync + std::fmt::Debug {
    /// Handle one notification. Errors are logged by the caller; a
    /// notification never blocks the stream.
    async fn handle_notification(&self, method: &str, params: Option<&Value>);
}

/// A JSON-RPC notification (no ID, no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// JSON-RPC version.
    pub jsonrpc: String,
    /// Method name.
    pub method: String,
    /// Parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A channel for JSON-RPC communication over stdio with a subprocess.
pub struct JsonRpcChannel {
    /// The child process (stdio transport only).
    process: Option<Child>,
    /// Remote HTTP/SSE transport, when configured.
    http: Option<crate::http::HttpChannel>,
    /// Stdin handle for writing requests.
    stdin: Option<ChildStdin>,
    /// Stdout reader for reading responses.
    stdout: Option<BufReader<ChildStdout>>,
    /// Next request ID counter.
    next_id: AtomicU64,
    /// Server command.
    command: String,
    /// Server arguments.
    args: Vec<String>,
    /// Environment variables.
    env: HashMap<String, String>,
    /// Working directory for the server, when configured.
    cwd: Option<std::path::PathBuf>,
    /// Sandbox policy for the server, when one applies.
    sandbox_config: Option<coderex_core::sandbox::SandboxConfig>,
    /// The sandbox handle. It owns the child's Job Object (Windows), so it
    /// must live exactly as long as the process.
    sandbox: Option<coderex_core::sandbox::SandboxHandle>,
    /// Whether the channel is initialized.
    initialized: bool,
    /// Answers requests the server makes of the client, when one was supplied.
    handler: Option<std::sync::Arc<dyn InboundRequestHandler>>,
    /// Receives server notifications, when one was supplied.
    notification_handler: Option<std::sync::Arc<dyn ServerNotificationHandler>>,
}

impl std::fmt::Debug for JsonRpcChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonRpcChannel")
            .field("command", &self.command)
            .field("initialized", &self.initialized)
            .field("has_handler", &self.handler.is_some())
            .field("http", &self.http.is_some())
            .finish_non_exhaustive()
    }
}

impl JsonRpcChannel {
    /// Create a new channel (not yet spawned).
    #[must_use]
    pub fn new(
        command: impl Into<String>,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            process: None,
            http: None,
            stdin: None,
            stdout: None,
            next_id: AtomicU64::new(1),
            command: command.into(),
            args,
            env,
            cwd: None,
            sandbox_config: None,
            sandbox: None,
            initialized: false,
            handler: None,
            notification_handler: None,
        }
    }

    /// Create a streamable-HTTP channel.
    pub fn new_http(url: &str, headers: HashMap<String, String>) -> Result<Self, String> {
        Ok(Self {
            process: None,
            http: Some(crate::http::HttpChannel::new(url, headers)?),
            stdin: None,
            stdout: None,
            next_id: AtomicU64::new(1),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            sandbox_config: None,
            sandbox: None,
            initialized: false,
            handler: None,
            notification_handler: None,
        })
    }

    /// Create a legacy HTTP+SSE channel.
    ///
    /// The long-lived GET stream announces the separate POST message endpoint;
    /// responses arrive there, correlated by JSON-RPC id.
    pub fn new_sse(url: &str, headers: HashMap<String, String>) -> Result<Self, String> {
        Ok(Self {
            process: None,
            http: Some(crate::http::HttpChannel::new_sse(url, headers)?),
            stdin: None,
            stdout: None,
            next_id: AtomicU64::new(1),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            sandbox_config: None,
            sandbox: None,
            initialized: false,
            handler: None,
            notification_handler: None,
        })
    }

    /// Configure the working directory and the sandbox for a stdio server.
    ///
    /// A `None` policy runs the server unwrapped. The channel keeps the
    /// resulting handle for the process's lifetime.
    pub fn set_sandbox(
        &mut self,
        cwd: Option<std::path::PathBuf>,
        config: Option<coderex_core::sandbox::SandboxConfig>,
    ) {
        self.cwd = cwd;
        self.sandbox_config = config;
    }

    /// Answer requests the server makes of the client.
    pub fn set_inbound_handler(&mut self, handler: std::sync::Arc<dyn InboundRequestHandler>) {
        // The remote transport's standalone GET stream runs in its own task,
        // so it needs its own owned handle to the same handler.
        if let Some(http) = self.http.as_mut() {
            http.set_event_stream_handler(std::sync::Arc::clone(&handler));
        }
        self.handler = Some(handler);
    }

    /// Receive notifications the server sends, on either transport.
    pub fn set_notification_handler(
        &mut self,
        handler: std::sync::Arc<dyn ServerNotificationHandler>,
    ) {
        if let Some(http) = self.http.as_mut() {
            // The GET-stream task and every POST response share the sink.
            http.set_notification_handler(std::sync::Arc::clone(&handler));
        }
        self.notification_handler = Some(handler);
    }

    /// How hard the standalone GET stream retries before it degrades.
    ///
    /// A stdio channel has no such stream, so this is a no-op there.
    pub const fn set_event_stream_retry_policy(
        &mut self,
        policy: crate::http::EventStreamRetryPolicy,
    ) {
        if let Some(http) = self.http.as_mut() {
            http.set_event_stream_retry_policy(policy);
        }
    }

    /// How often a degraded standalone GET stream probes for recovery.
    ///
    /// `None` leaves a degraded stream stopped. A stdio channel has no such
    /// stream, so this is a no-op there.
    pub const fn set_event_stream_probe_interval(&mut self, interval: Option<std::time::Duration>) {
        if let Some(http) = self.http.as_mut() {
            http.set_event_stream_probe_interval(interval);
        }
    }

    /// The standalone GET stream's state, if this is a remote channel.
    #[must_use]
    pub fn event_stream_state(&self) -> Option<crate::http::EventStreamState> {
        self.http
            .as_ref()
            .map(crate::http::HttpChannel::event_stream_state)
    }

    /// Stop the standalone GET stream and wait for its task, if any.
    pub async fn stop_event_stream(&mut self) {
        if let Some(http) = self.http.as_mut() {
            http.stop_event_stream().await;
        }
    }

    /// Hand a notification to the installed sink.
    async fn dispatch_notification(&self, method: &str, params: Option<&Value>) {
        if let Some(handler) = &self.notification_handler {
            handler.handle_notification(method, params).await;
        } else {
            tracing::debug!(
                server = %self.command,
                method,
                "ignoring a notification from the server"
            );
        }
    }

    /// Resolve the credential before each HTTP request.
    ///
    /// Only the remote transport has a credential to resolve; on a stdio
    /// channel this is a no-op, because a subprocess is authenticated by its
    /// environment rather than by an HTTP header.
    pub fn set_token_provider(&mut self, provider: std::sync::Arc<dyn crate::http::TokenProvider>) {
        if let Some(http) = self.http.as_mut() {
            http.set_token_provider(provider);
        }
    }

    /// Spawn the subprocess and capture stdio handles.
    ///
    /// The body has nothing to await, but every transport's spawn is a future
    /// to its callers, so the `async` signature stays.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn spawn(&mut self) -> Result<(), String> {
        if let Some(http) = self.http.as_ref() {
            // Legacy SSE has no request channel until its GET stream announces
            // the POST endpoint, so the stream must be running before the
            // initialize request can be sent.
            if http.is_legacy_sse() {
                http.start_event_stream();
            }
            return Ok(());
        }

        // A server named by a path that lands in the working directory is a
        // checkout deciding what the client launches; refuse it rather than
        // run it.
        let cwd = std::env::current_dir().unwrap_or_default();
        if coderex_core::sandbox::resolves_inside_cwd(&self.command, &cwd) {
            return Err(format!(
                "refusing to spawn '{}': it resolves inside the working directory {}",
                self.command,
                cwd.display()
            ));
        }

        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args);
        // The server is not a provider client, so it does not inherit this
        // session's credentials. Its own configured environment is layered on
        // afterwards and always wins, so a server that genuinely needs a
        // variable gets it by naming it in its configuration.
        for key in coderex_core::sandbox::credential_scrub_keys() {
            if !self.env.contains_key(&key) {
                cmd.env_remove(&key);
            }
        }
        cmd.envs(&self.env);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        // On Windows the server is confined by an AppContainer token plus a
        // Job Object. `prepare` creates it suspended, and `post_spawn_raw`
        // attaches the token, assigns the job and resumes it. A failure is a
        // refusal: a server that was meant to be confined does not run
        // unconfined.
        #[cfg(target_os = "windows")]
        let sandbox = match self.sandbox_config.as_ref() {
            Some(config) => {
                let handle = coderex_core::sandbox::SandboxHandle::create(config.clone())
                    .map_err(|e| format!("could not sandbox '{}': {e}", self.command))?;
                handle
                    .prepare(cmd.as_std_mut())
                    .map_err(|e| format!("could not prepare '{}': {e}", self.command))?;
                Some(handle)
            }
            None => None,
        };

        let mut process = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn '{}': {e}", self.command))?;

        #[cfg(target_os = "windows")]
        if let Some(handle) = &sandbox {
            let Some(raw) = process.raw_handle() else {
                let _ = process.kill().await;
                return Err(format!(
                    "the spawned MCP server '{}' has no process handle, so it could not be \
                     confined; refusing to run it",
                    self.command
                ));
            };
            if let Err(error) = handle.post_spawn_raw(raw) {
                let _ = process.kill().await;
                return Err(format!(
                    "could not confine the MCP server '{}': {error}",
                    self.command
                ));
            }
        }
        #[cfg(target_os = "windows")]
        {
            self.sandbox = sandbox;
        }

        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| "failed to take stdin from child process".to_string())?;
        let stdout = process
            .stdout
            .take()
            .ok_or_else(|| "failed to take stdout from child process".to_string())?;

        self.process = Some(process);
        self.stdin = Some(stdin);
        self.stdout = Some(BufReader::new(stdout));
        Ok(())
    }

    /// Perform the MCP initialize/initialized handshake.
    pub async fn initialize(&mut self) -> Result<Value, String> {
        // Send initialize request
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "coderex",
                "version": "0.1.0"
            }
        });

        // Legacy SSE cannot send a request until its GET stream has announced
        // the POST endpoint. Wait for the connection here so a non-SSE
        // endpoint is rejected immediately rather than after a request
        // timeout.
        if let Some(ref http) = self.http {
            if http.is_legacy_sse() {
                if http.event_stream_state() == crate::http::EventStreamState::Stopped {
                    http.start_event_stream();
                }
                http.await_legacy_stream().await?;
            }
        }

        let result = self.send_request("initialize", Some(init_params)).await?;

        // Send initialized notification (no response expected). The initialize
        // response may have assigned a session id; every later POST carries it.
        self.send_notification("initialized", None).await?;
        self.initialized = true;

        // The standalone server-to-client stream is only meaningful once the
        // session exists. A server that does not support it answers 405 and
        // the stream task stops quietly. Legacy SSE already started its
        // mandatory stream before initialize; restarting it here would drop
        // the session that just answered the handshake.
        if let Some(ref http) = self.http {
            if !http.is_legacy_sse() {
                http.start_event_stream();
            }
        }

        Ok(result)
    }

    /// The remote session id, once the server has assigned one.
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.http
            .as_ref()
            .and_then(crate::http::HttpChannel::session_id)
    }

    /// Whether the remote standalone GET stream is running.
    #[must_use]
    pub fn event_stream_active(&self) -> bool {
        self.http
            .as_ref()
            .is_some_and(crate::http::HttpChannel::event_stream_active)
    }

    /// Discover tools from the MCP server.
    pub async fn list_tools(&mut self) -> Result<Value, String> {
        self.send_request("tools/list", None).await
    }

    /// Discover resources from the MCP server.
    pub async fn list_resources(&mut self) -> Result<Value, String> {
        self.send_request("resources/list", None).await
    }

    /// Read one resource from the MCP server.
    ///
    /// Returns the raw `resources/read` JSON response, whose `contents` array
    /// carries either `text` or base64 `blob` entries.
    pub async fn read_resource(&mut self, uri: &str) -> Result<Value, String> {
        self.send_request("resources/read", Some(serde_json::json!({"uri": uri})))
            .await
    }

    /// Send a JSON-RPC request and wait for the response.
    pub async fn send_request(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };

        if let Some(ref http) = self.http {
            // A remote server can ask the client things too; the handler is
            // the same one the stdio read loop uses.
            return http.send_request(&request, self.handler.as_deref()).await;
        }

        self.write_request(&request).await?;
        self.read_response(id).await
    }

    /// Send a JSON-RPC notification (no response expected).
    pub async fn send_notification(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), String> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        };
        if let Some(ref http) = self.http {
            return http.send_notification(&notification).await;
        }

        self.write_notification(&notification).await
    }

    /// Write a request to stdin.
    async fn write_request(&mut self, request: &JsonRpcRequest) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("stdin not available")?;
        let line = serde_json::to_string(&request)
            .map_err(|e| format!("failed to serialize request: {e}"))?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| format!("failed to write to stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("failed to flush stdin: {e}"))?;
        Ok(())
    }

    /// Write a notification to stdin.
    async fn write_notification(
        &mut self,
        notification: &JsonRpcNotification,
    ) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("stdin not available")?;
        let line = serde_json::to_string(&notification)
            .map_err(|e| format!("failed to serialize notification: {e}"))?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| format!("failed to write to stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("failed to flush stdin: {e}"))?;
        Ok(())
    }

    /// Read a response from stdout matching the given ID.
    async fn read_response(&mut self, expected_id: u64) -> Result<Value, String> {
        let mut line = String::new();

        loop {
            line.clear();
            // The borrow of `stdout` ends with the read, so answering a server
            // request can take the channel again to write.
            let bytes_read = {
                let stdout = self.stdout.as_mut().ok_or("stdout not available")?;
                stdout
                    .read_line(&mut line)
                    .await
                    .map_err(|e| format!("failed to read from stdout: {e}"))?
            };

            if bytes_read == 0 {
                return Err("stdout closed before response".to_string());
            }

            let trimmed = line.trim().to_string();
            let trimmed = trimmed.as_str();
            if trimmed.is_empty() {
                continue;
            }

            // Told apart by shape, not by trying to read every line as a
            // response: the protocol runs both ways, and a request from the
            // server has a method and an id but no result.
            let value: Value = serde_json::from_str(trimmed)
                .map_err(|e| format!("failed to parse JSON-RPC message: {e}, raw: {trimmed}"))?;
            let has_method = value.get("method").is_some();
            let has_id = value.get("id").is_some();

            match (has_method, has_id) {
                // The server asking us to do something. It is waiting for an
                // answer, so one is always sent — an error if nothing here can
                // answer it.
                (true, true) => {
                    let request: JsonRpcServerRequest = serde_json::from_value(value)
                        .map_err(|e| format!("failed to parse a server request: {e}"))?;
                    self.answer_server_request(&request).await?;
                    continue;
                }
                // A notification: no answer is expected, but it can ask the
                // manager to re-discover tools or invalidate a cached resource.
                (true, false) => {
                    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
                    self.dispatch_notification(method, value.get("params"))
                        .await;
                    continue;
                }
                (false, true) => {}
                // Neither shape: say so rather than read past it.
                (false, false) => {
                    tracing::warn!(
                        server = %self.command,
                        message = %trimmed,
                        "a message from the server is neither a response nor a request"
                    );
                    continue;
                }
            }

            let response: JsonRpcResponse = serde_json::from_value(value)
                .map_err(|e| format!("failed to parse JSON-RPC response: {e}, raw: {trimmed}"))?;

            if response.id == expected_id {
                if let Some(error) = response.error {
                    return Err(format!(
                        "JSON-RPC error (code {}): {}",
                        error.code, error.message
                    ));
                }
                return Ok(response.result.unwrap_or(Value::Null));
            }

            // An answer to a request that is no longer being waited for — a
            // retry, or one that timed out. Read past it, but not silently.
            tracing::debug!(
                server = %self.command,
                id = response.id,
                expected = expected_id,
                "a response arrived for a request nothing is waiting on"
            );
        }
    }

    /// Answer a request the server made.
    async fn answer_server_request(
        &mut self,
        request: &JsonRpcServerRequest,
    ) -> Result<(), String> {
        let answer = match &self.handler {
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

        self.write_response(&response).await
    }

    /// Write a response to a request the server made.
    async fn write_response(&mut self, response: &JsonRpcResponse) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("stdin not available")?;
        let line = serde_json::to_string(response)
            .map_err(|e| format!("failed to serialize response: {e}"))?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| format!("failed to write to stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("failed to flush stdin: {e}"))?;
        Ok(())
    }

    /// Check if the channel is alive by attempting to read from stdout.
    pub async fn ping(&mut self) -> Result<(), String> {
        if self.http.is_some() {
            return self.send_request("ping", None).await.map(|_| ());
        }

        // For MCP servers, we check if the process is still running
        if let Some(ref mut process) = self.process {
            match process.try_wait() {
                Ok(Some(status)) => {
                    return Err(format!("process exited with status: {status}"));
                }
                Ok(None) => {} // Still running
                Err(e) => {
                    return Err(format!("failed to check process status: {e}"));
                }
            }
        } else {
            return Err("process not spawned".to_string());
        }

        Ok(())
    }

    /// Check if initialized.
    #[must_use]
    pub const fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Check if the process is still running.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.process.is_some() || self.http.is_some()
    }

    /// Terminate the process.
    pub async fn terminate(&mut self) -> Result<(), String> {
        if let Some(http) = self.http.take() {
            // Stop the standalone stream before dropping the channel; a
            // detached task would otherwise keep polling a server that the
            // session has finished with. Awaiting it means the response body
            // is closed and the task is gone when shutdown returns.
            http.stop_event_stream().await;
            self.initialized = false;
            return Ok(());
        }

        if let Some(ref mut process) = self.process {
            // Kill the whole tree first: a server that spawns children (a
            // launcher that runs the real server) would otherwise leave them
            // behind.
            if let Some(handle) = self.sandbox.take() {
                let _ = handle.terminate();
            }
            process
                .kill()
                .await
                .map_err(|e| format!("failed to kill process: {e}"))?;
            let _ = process.wait().await;
        }
        self.sandbox = None;
        self.process = None;
        self.stdin = None;
        self.stdout = None;
        self.initialized = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_channel_not_initialized() {
        let channel = JsonRpcChannel::new("echo", vec![], HashMap::new());
        assert!(!channel.is_initialized());
        assert!(!channel.is_running());
    }

    #[tokio::test]
    async fn spawn_valid_process() {
        // Use a command that stays alive briefly (cat reads from stdin)
        let mut channel = JsonRpcChannel::new("cat", vec![], HashMap::new());
        let result = channel.spawn().await;
        assert!(result.is_ok(), "spawn failed: {result:?}");
        assert!(channel.is_running());
        // Clean up
        let _ = channel.terminate().await;
    }

    #[tokio::test]
    async fn spawn_invalid_command_fails() {
        let mut channel = JsonRpcChannel::new("nonexistent_command_xyz", vec![], HashMap::new());
        let result = channel.spawn().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ping_before_spawn_fails() {
        let mut channel = JsonRpcChannel::new("echo", vec![], HashMap::new());
        let result = channel.ping().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ping_after_spawn_succeeds() {
        let mut channel = JsonRpcChannel::new("cat", vec![], HashMap::new());
        channel.spawn().await.unwrap();
        let result = channel.ping().await;
        assert!(result.is_ok());
        let _ = channel.terminate().await;
    }

    #[tokio::test]
    async fn terminate_clears_state() {
        let mut channel = JsonRpcChannel::new("cat", vec![], HashMap::new());
        channel.spawn().await.unwrap();
        assert!(channel.is_running());
        channel.terminate().await.unwrap();
        assert!(!channel.is_running());
        assert!(!channel.is_initialized());
    }

    #[tokio::test]
    async fn request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 42,
            method: "tools/call".to_string(),
            params: Some(serde_json::json!({"name": "read_file"})),
        };
        let serialized = serde_json::to_string(&req).unwrap();
        let parsed: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["method"], "tools/call");
    }

    #[tokio::test]
    async fn response_parsing() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"hello"}]}}"#;
        let response: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, 1);
        assert!(response.result.is_some());
        assert!(response.error.is_none());
    }

    #[tokio::test]
    async fn error_response() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#;
        let response: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(response.error.is_some());
        let err = response.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
    }

    /// Read one line of the spawned child's stdout.
    #[cfg(unix)]
    async fn read_child_line(channel: &mut JsonRpcChannel) -> String {
        use tokio::io::AsyncBufReadExt as _;
        let mut line = String::new();
        channel
            .stdout
            .as_mut()
            .expect("stdout")
            .read_line(&mut line)
            .await
            .expect("read child stdout");
        line.trim_end().to_string()
    }

    /// An MCP server is not a provider client: it must not inherit the
    /// session's credentials.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(unsafe_code)]
    async fn spawn_does_not_hand_over_provider_credentials() {
        let key = "ANTHROPIC_AUTH_TOKEN";
        let previous = std::env::var_os(key);
        // SAFETY: edition 2024 marks `set_var` unsafe because another thread
        // could read the environment concurrently. This test names a variable
        // no other test in this crate reads.
        unsafe { std::env::set_var(key, "coderex-test-secret") };

        let mut channel = JsonRpcChannel::new(
            "sh",
            vec![
                "-c".to_string(),
                "printf '%s\\n' \"${ANTHROPIC_AUTH_TOKEN:-STRIPPED}\"".to_string(),
            ],
            HashMap::new(),
        );
        let spawned = channel.spawn().await;
        let seen = match &spawned {
            Ok(()) => read_child_line(&mut channel).await,
            Err(e) => e.clone(),
        };
        let _ = channel.terminate().await;

        match previous {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }

        spawned.expect("spawn");
        assert_eq!(seen, "STRIPPED", "credential reached the MCP server");
    }

    /// The server's own configured environment still arrives — including a
    /// credential it explicitly asked for.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_passes_the_configured_environment() {
        let mut env = HashMap::new();
        env.insert("CODREX_MCP_PLAIN".to_string(), "from-config".to_string());
        env.insert(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            "from-config".to_string(),
        );

        let mut channel = JsonRpcChannel::new(
            "sh",
            vec![
                "-c".to_string(),
                "printf '%s %s\\n' \"$CODREX_MCP_PLAIN\" \"$ANTHROPIC_AUTH_TOKEN\"".to_string(),
            ],
            env,
        );
        channel.spawn().await.expect("spawn");
        let seen = read_child_line(&mut channel).await;
        let _ = channel.terminate().await;

        assert_eq!(seen, "from-config from-config");
    }

    /// A server command that would come out of the working directory is
    /// refused instead of run.
    #[tokio::test]
    async fn spawn_refuses_a_command_inside_the_working_directory() {
        let cwd = std::env::current_dir().unwrap();
        let name = format!("coderex_planted_mcp_{}", std::process::id());
        let planted = cwd.join(&name);
        std::fs::write(&planted, b"#!/bin/sh\nexit 0\n").unwrap();

        let mut channel = JsonRpcChannel::new(format!("./{name}"), vec![], HashMap::new());
        let result = channel.spawn().await;

        let _ = std::fs::remove_file(&planted);

        let error = result.expect_err("a program inside the cwd must be refused");
        assert!(
            error.contains("resolves inside the working directory"),
            "unexpected error: {error}"
        );
    }

    /// A stdio server runs inside the AppContainer: it can write in its
    /// workspace and nowhere else.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_stdio_channel_is_sandboxed() {
        if !coderex_core::sandbox::app_container_available() {
            assert!(
                std::env::var("CODEREX_REQUIRE_APP_CONTAINER").as_deref() != Ok("1"),
                "AppContainer is unavailable on this runner but \
                 CODEREX_REQUIRE_APP_CONTAINER=1"
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("workspace");
        let denied = dir.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&denied).unwrap();

        // No inner quotes: the command is a single argv entry, and an
        // escaped quote (`\"`) is literal to cmd. These paths contain no
        // spaces, so plain names are valid.
        let script = format!(
            "echo inside> {} & echo outside> {}",
            allowed.join("inside.txt").display(),
            denied.join("outside.txt").display()
        );
        let mut channel =
            JsonRpcChannel::new("cmd.exe", vec!["/C".to_string(), script], HashMap::new());
        channel.set_sandbox(
            Some(allowed.clone()),
            Some(coderex_core::sandbox::SandboxConfig {
                allowed_read_paths: vec![allowed.clone()],
                allowed_write_paths: vec![allowed.clone()],
                allowed_exec_paths: Vec::new(),
                additional_working_dirs: vec![allowed.clone()],
                ..coderex_core::sandbox::SandboxConfig::default()
            }),
        );

        tokio::time::timeout(std::time::Duration::from_secs(20), channel.spawn())
            .await
            .expect("the sandboxed server never started; was the child resumed?")
            .expect("the server should spawn");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !allowed.join("inside.txt").exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            allowed.join("inside.txt").exists(),
            "the server could not write in its workspace"
        );
        assert!(
            !denied.join("outside.txt").exists(),
            "the server wrote outside its policy"
        );
        let _ = channel.terminate().await;
    }
}
