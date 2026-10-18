//! MCP server lifecycle — stdio JSON-RPC server spawn and management.

use crate::channel::JsonRpcChannel;
use crate::config::McpTransport;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

/// Errors from MCP server operations.
#[derive(Debug, Error)]
pub enum McpServerError {
    /// Failed to spawn the MCP server process.
    #[error("failed to spawn server '{name}': {source}")]
    Spawn {
        /// The server name.
        name: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The named MCP server was not found.
    #[error("server '{name}' not found")]
    NotFound {
        /// The name of the server.
        name: String,
    },
    /// Server already running.
    #[error("server '{name}' is already running")]
    AlreadyRunning {
        /// The server name.
        name: String,
    },
    /// Health check failed.
    #[error("health check failed for '{name}': {reason}")]
    HealthCheckFailed {
        /// The server name.
        name: String,
        /// Reason for failure.
        reason: String,
    },
}

/// Status of an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    /// Not yet started.
    Stopped,
    /// Starting up.
    Starting,
    /// Running and healthy.
    Running,
    /// Unhealthy — may reconnect.
    Unhealthy,
    /// Stopped after error.
    Errored,
}

/// A running MCP server instance.
#[derive(Debug, Clone)]
pub struct McpServer {
    /// Server name/identifier.
    pub name: String,
    /// Command that was/will be executed.
    pub command: String,
    /// Arguments for the command.
    pub args: Vec<String>,
    /// Environment variables.
    pub env: HashMap<String, String>,
    /// Transport used by this server.
    pub transport: McpTransport,
    /// Remote endpoint for HTTP/SSE transports.
    pub url: Option<String>,
    /// Additional HTTP headers for remote transports.
    pub headers: HashMap<String, String>,
    /// Current status.
    pub status: ServerStatus,
    /// Number of reconnection attempts.
    pub reconnect_count: usize,
    /// Maximum reconnection attempts.
    pub max_reconnects: usize,
    /// JSON-RPC stdio channel (present when running).
    channel: Option<Arc<Mutex<JsonRpcChannel>>>,
    /// Answers requests this server makes of the client.
    ///
    /// A shared slot rather than a plain field: the manager owns one slot for
    /// every server, so a session can install its UI handler after the servers
    /// have already been spawned, and a reconnecting server keeps it.
    inbound_handler: crate::channel::SharedInboundHandler,
    /// Resolves the bearer token before each remote request, when installed.
    token_provider: Option<Arc<dyn crate::http::TokenProvider>>,
    /// Receives notifications the server pushes, when installed.
    ///
    /// Held on the server for the same reason as the inbound handler: a
    /// reconnect builds a fresh channel, which must keep the sink.
    notification_handler: Option<Arc<dyn crate::channel::ServerNotificationHandler>>,
    /// Retry settings for the standalone GET stream, when changed from the
    /// default.
    event_stream_retry_policy: Option<crate::http::EventStreamRetryPolicy>,
}

impl McpServer {
    /// Create a new server definition (not yet spawned).
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args,
            env,
            transport: McpTransport::Stdio,
            url: None,
            headers: HashMap::new(),
            status: ServerStatus::Stopped,
            reconnect_count: 0,
            max_reconnects: 3,
            channel: None,
            inbound_handler: crate::channel::SharedInboundHandler::new(),
            token_provider: None,
            notification_handler: None,
            event_stream_retry_policy: None,
        }
    }

    /// Create a remote HTTP or SSE server definition.
    #[must_use]
    pub fn new_http(
        name: impl Into<String>,
        url: impl Into<String>,
        transport: McpTransport,
        headers: HashMap<String, String>,
    ) -> Self {
        Self {
            name: name.into(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            transport,
            url: Some(url.into()),
            headers,
            status: ServerStatus::Stopped,
            reconnect_count: 0,
            max_reconnects: 3,
            channel: None,
            inbound_handler: crate::channel::SharedInboundHandler::new(),
            token_provider: None,
            notification_handler: None,
            event_stream_retry_policy: None,
        }
    }

    /// Build a fresh channel using this server's transport configuration.
    fn create_channel(&self) -> Result<JsonRpcChannel, String> {
        // The handler belongs to the server rather than to a channel: `spawn`
        // builds the channel it will use, so one set on the pre-spawn handle
        // would be dropped on the floor.
        let mut channel = match self.transport {
            McpTransport::Stdio => {
                JsonRpcChannel::new(self.command.clone(), self.args.clone(), self.env.clone())
            }
            McpTransport::Sse => {
                let url = self.url.as_deref().ok_or_else(|| {
                    format!("remote MCP server '{}' has no URL configured", self.name)
                })?;
                JsonRpcChannel::new_sse(url, self.headers.clone())?
            }
            McpTransport::Http => {
                let url = self.url.as_deref().ok_or_else(|| {
                    format!("remote MCP server '{}' has no URL configured", self.name)
                })?;
                JsonRpcChannel::new_http(url, self.headers.clone())?
            }
        };
        // Always install the shared slot. When it is empty it refuses server
        // requests with `-32601`, which is the honest answer; when the session
        // fills it later, this channel sees the change without being rebuilt.
        channel.set_inbound_handler(Arc::new(self.inbound_handler.clone()));
        if let Some(ref provider) = self.token_provider {
            channel.set_token_provider(Arc::clone(provider));
        }
        if let Some(ref handler) = self.notification_handler {
            channel.set_notification_handler(Arc::clone(handler));
        }
        if let Some(policy) = self.event_stream_retry_policy {
            channel.set_event_stream_retry_policy(policy);
        }
        Ok(channel)
    }

    /// Receive notifications the server pushes (`tools/list_changed`, …).
    pub fn set_notification_handler(
        &mut self,
        handler: Arc<dyn crate::channel::ServerNotificationHandler>,
    ) {
        self.notification_handler = Some(handler);
    }

    /// Change how hard the standalone GET stream retries before it degrades.
    pub fn set_event_stream_retry_policy(&mut self, policy: crate::http::EventStreamRetryPolicy) {
        self.event_stream_retry_policy = Some(policy);
        if let Some(channel) = &self.channel {
            // Best effort: the channel may be locked by an in-flight request,
            // in which case the stored policy still applies to a reconnect.
            if let Ok(mut channel) = channel.try_lock() {
                channel.set_event_stream_retry_policy(policy);
            }
        }
    }

    /// The standalone GET stream's state, when this is a remote server that is
    /// running.
    pub async fn event_stream_state(&self) -> Option<crate::http::EventStreamState> {
        let channel = self.channel.as_ref()?;
        let channel = channel.lock().await;
        channel.event_stream_state()
    }

    /// Resolve the credential for this server before every remote request.
    ///
    /// The provider is stored on the server rather than on a channel because
    /// [`McpServer::spawn`] and [`McpServer::reconnect`] build a fresh channel;
    /// a provider set on a pre-spawn handle would otherwise be dropped.
    pub fn set_token_provider(&mut self, provider: Arc<dyn crate::http::TokenProvider>) {
        self.token_provider = Some(provider);
    }

    /// The credential resolver installed on this server, if any.
    #[must_use]
    pub fn token_provider(&self) -> Option<&Arc<dyn crate::http::TokenProvider>> {
        self.token_provider.as_ref()
    }

    /// Answer requests this server makes of the client.
    ///
    /// A server that asks — `elicitation/create` is the common one — waits for
    /// the answer, so a client that cannot answer must say so. Without a
    /// handler every such request is refused with a JSON-RPC error rather than
    /// ignored.
    pub fn set_inbound_handler(&mut self, handler: Arc<dyn crate::channel::InboundRequestHandler>) {
        self.inbound_handler.install(handler);
    }

    /// Share `handler` with this server instead of using its own slot.
    ///
    /// The manager uses this so one install covers every server, including
    /// ones that reconnect later.
    pub(crate) fn share_inbound_handler(&mut self, handler: crate::channel::SharedInboundHandler) {
        self.inbound_handler = handler;
    }

    /// Spawn the server process and initialize JSON-RPC channel.
    pub async fn spawn(&mut self) -> Result<(), McpServerError> {
        if self.status == ServerStatus::Running {
            return Err(McpServerError::AlreadyRunning {
                name: self.name.clone(),
            });
        }
        self.status = ServerStatus::Starting;

        let mut channel = self.create_channel().map_err(|error| {
            self.status = ServerStatus::Errored;
            McpServerError::Spawn {
                name: self.name.clone(),
                source: std::io::Error::new(std::io::ErrorKind::Other, error),
            }
        })?;

        channel.spawn().await.map_err(|e| {
            self.status = ServerStatus::Errored;
            McpServerError::Spawn {
                name: self.name.clone(),
                source: std::io::Error::new(std::io::ErrorKind::Other, e.clone()),
            }
        })?;

        // Perform MCP initialize handshake
        match channel.initialize().await {
            Ok(_) => {
                tracing::debug!(server = %self.name, "MCP server initialized");
            }
            Err(e) => {
                // Classic SSE has no usable transport until the handshake
                // succeeds: the GET stream must announce the POST endpoint.
                // Refusing the server is honest; continuing would leave a
                // "running" server that can never send a request.
                if self.transport == McpTransport::Sse {
                    self.status = ServerStatus::Errored;
                    return Err(McpServerError::Spawn {
                        name: self.name.clone(),
                        source: std::io::Error::new(std::io::ErrorKind::Other, e),
                    });
                }
                // Some other servers may not support full MCP initialize — log
                // but continue.
                tracing::warn!(server = %self.name, "MCP initialize failed (continuing anyway): {e}");
            }
        }

        self.channel = Some(Arc::new(Mutex::new(channel)));
        self.status = ServerStatus::Running;
        Ok(())
    }

    /// Run a health check via process liveness (immutable).
    pub async fn health_check(&self) -> Result<(), McpServerError> {
        if self.status != ServerStatus::Running {
            return Err(McpServerError::HealthCheckFailed {
                name: self.name.clone(),
                reason: "server not running".to_string(),
            });
        }

        if let Some(ref channel) = self.channel {
            let mut ch = channel.lock().await;
            ch.ping()
                .await
                .map_err(|e| McpServerError::HealthCheckFailed {
                    name: self.name.clone(),
                    reason: e,
                })?;
        } else {
            return Err(McpServerError::HealthCheckFailed {
                name: self.name.clone(),
                reason: "no channel".to_string(),
            });
        }

        Ok(())
    }

    /// Update server status (for health monitor).
    pub fn set_status(&mut self, status: ServerStatus) {
        self.status = status;
    }

    /// Discover tools from the MCP server.
    ///
    /// Returns the raw `tools/list` JSON response.
    pub async fn discover_tools(&mut self) -> Result<serde_json::Value, McpServerError> {
        if self.status != ServerStatus::Running {
            return Err(McpServerError::HealthCheckFailed {
                name: self.name.clone(),
                reason: "server not running".to_string(),
            });
        }

        let channel_opt = self.channel.take();
        let result = if let Some(ref channel) = channel_opt {
            let mut ch = channel.lock().await;
            ch.list_tools()
                .await
                .map_err(|e| McpServerError::HealthCheckFailed {
                    name: self.name.clone(),
                    reason: e,
                })
        } else {
            Err(McpServerError::HealthCheckFailed {
                name: self.name.clone(),
                reason: "no channel".to_string(),
            })
        };

        if let Some(channel) = channel_opt {
            self.channel = Some(channel);
        }
        result
    }

    /// Discover resources from the MCP server.
    ///
    /// Returns the raw `resources/list` JSON response.
    pub async fn discover_resources(&mut self) -> Result<serde_json::Value, McpServerError> {
        if self.status != ServerStatus::Running {
            return Err(McpServerError::HealthCheckFailed {
                name: self.name.clone(),
                reason: "server not running".to_string(),
            });
        }

        let channel_opt = self.channel.take();
        let result = if let Some(ref channel) = channel_opt {
            let mut ch = channel.lock().await;
            ch.list_resources()
                .await
                .map_err(|e| McpServerError::HealthCheckFailed {
                    name: self.name.clone(),
                    reason: e,
                })
        } else {
            Err(McpServerError::HealthCheckFailed {
                name: self.name.clone(),
                reason: "no channel".to_string(),
            })
        };

        if let Some(channel) = channel_opt {
            self.channel = Some(channel);
        }
        result
    }

    /// Attempt reconnection — terminate old channel and spawn new one.
    pub async fn reconnect(&mut self) -> Result<(), McpServerError> {
        if self.reconnect_count >= self.max_reconnects {
            self.status = ServerStatus::Errored;
            return Err(McpServerError::HealthCheckFailed {
                name: self.name.clone(),
                reason: format!("max reconnects ({}) reached", self.max_reconnects),
            });
        }
        self.reconnect_count += 1;
        self.status = ServerStatus::Starting;

        // Terminate old channel if present
        if let Some(ref channel) = self.channel {
            let mut ch = channel.lock().await;
            let _ = ch.terminate().await;
        }
        self.channel = None;

        // Spawn fresh channel
        let mut channel = self.create_channel().map_err(|error| {
            self.status = ServerStatus::Errored;
            McpServerError::Spawn {
                name: self.name.clone(),
                source: std::io::Error::new(std::io::ErrorKind::Other, error),
            }
        })?;

        channel.spawn().await.map_err(|e| {
            self.status = ServerStatus::Errored;
            McpServerError::Spawn {
                name: self.name.clone(),
                source: std::io::Error::new(std::io::ErrorKind::Other, e.clone()),
            }
        })?;

        match channel.initialize().await {
            Ok(_) => {}
            Err(error) if self.transport == McpTransport::Sse => {
                // The stream did not announce a message endpoint; the
                // reconnected server cannot be used.
                self.status = ServerStatus::Errored;
                return Err(McpServerError::Spawn {
                    name: self.name.clone(),
                    source: std::io::Error::new(std::io::ErrorKind::Other, error),
                });
            }
            Err(error) => {
                tracing::warn!(
                    server = %self.name,
                    "MCP initialize failed after reconnecting (continuing anyway): {error}"
                );
            }
        }
        self.channel = Some(Arc::new(Mutex::new(channel)));
        self.status = ServerStatus::Running;
        Ok(())
    }

    /// Stop the server and terminate channel.
    pub async fn stop(&mut self) {
        if let Some(ref channel) = self.channel {
            let mut ch = channel.lock().await;
            let _ = ch.terminate().await;
        }
        self.channel = None;
        self.status = ServerStatus::Stopped;
    }

    /// Get a reference to the JSON-RPC channel (if running).
    pub fn channel(&self) -> Option<Arc<Mutex<JsonRpcChannel>>> {
        self.channel.clone()
    }

    /// Check if the server can attempt another reconnection.
    #[must_use]
    pub fn can_reconnect(&self) -> bool {
        self.reconnect_count < self.max_reconnects
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_server() -> McpServer {
        McpServer::new("test", "cat", vec![], HashMap::new())
    }

    #[test]
    fn new_server_is_stopped() {
        let srv = sample_server();
        assert_eq!(srv.status, ServerStatus::Stopped);
        assert_eq!(srv.reconnect_count, 0);
    }

    #[tokio::test]
    async fn spawn_transitions_to_running() {
        let mut srv = sample_server();
        srv.spawn().await.unwrap();
        assert_eq!(srv.status, ServerStatus::Running);
        assert!(srv.channel().is_some());
        // Clean up
        srv.stop().await;
    }

    #[tokio::test]
    async fn spawn_already_running_errors() {
        let mut srv = sample_server();
        srv.spawn().await.unwrap();
        let result = srv.spawn().await;
        assert!(result.is_err());
        srv.stop().await;
    }

    #[tokio::test]
    async fn health_check_passes_when_running() {
        let mut srv = sample_server();
        srv.spawn().await.unwrap();
        srv.health_check().await.unwrap();
        srv.stop().await;
    }

    #[tokio::test]
    async fn health_check_fails_when_stopped() {
        let mut srv = sample_server();
        let result = srv.health_check().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reconnect_attempts_up_to_max() {
        let mut srv = sample_server();
        srv.max_reconnects = 2;

        srv.reconnect().await.unwrap();
        assert_eq!(srv.reconnect_count, 1);
        srv.reconnect().await.unwrap();
        assert_eq!(srv.reconnect_count, 2);

        let result = srv.reconnect().await;
        assert!(result.is_err());
        assert_eq!(srv.status, ServerStatus::Errored);
    }

    #[tokio::test]
    async fn stop_transitions_to_stopped() {
        let mut srv = sample_server();
        srv.spawn().await.unwrap();
        srv.stop().await;
        assert_eq!(srv.status, ServerStatus::Stopped);
        assert!(srv.channel().is_none());
    }

    #[test]
    fn can_reconnect() {
        let mut srv = sample_server();
        srv.max_reconnects = 1;
        assert!(srv.can_reconnect());
        srv.reconnect_count = 1;
        assert!(!srv.can_reconnect());
    }
}
