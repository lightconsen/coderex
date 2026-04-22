//! MCP server lifecycle — stdio JSON-RPC server spawn and management.

use crate::channel::JsonRpcChannel;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use thiserror::Error;

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
#[derive(Debug)]
pub struct McpServer {
    /// Server name/identifier.
    pub name: String,
    /// Command that was/will be executed.
    pub command: String,
    /// Arguments for the command.
    pub args: Vec<String>,
    /// Environment variables.
    pub env: HashMap<String, String>,
    /// Current status.
    pub status: ServerStatus,
    /// Number of reconnection attempts.
    pub reconnect_count: usize,
    /// Maximum reconnection attempts.
    pub max_reconnects: usize,
    /// JSON-RPC stdio channel (present when running).
    channel: Option<Arc<Mutex<JsonRpcChannel>>>,
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
            status: ServerStatus::Stopped,
            reconnect_count: 0,
            max_reconnects: 3,
            channel: None,
        }
    }

    /// Spawn the server process and initialize JSON-RPC channel.
    pub async fn spawn(&mut self) -> Result<(), McpServerError> {
        if self.status == ServerStatus::Running {
            return Err(McpServerError::AlreadyRunning { name: self.name.clone() });
        }
        self.status = ServerStatus::Starting;

        let mut channel = JsonRpcChannel::new(
            self.command.clone(),
            self.args.clone(),
            self.env.clone(),
        );

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
                // Some servers may not support full MCP initialize — log but continue
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
            ch.ping().await.map_err(|e| McpServerError::HealthCheckFailed {
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
    pub async fn discover_tools(
        &mut self,
    ) -> Result<serde_json::Value, McpServerError> {
        if self.status != ServerStatus::Running {
            return Err(McpServerError::HealthCheckFailed {
                name: self.name.clone(),
                reason: "server not running".to_string(),
            });
        }

        let channel_opt = self.channel.take();
        let result = if let Some(ref channel) = channel_opt {
            let mut ch = channel.lock().await;
            ch.list_tools().await.map_err(|e| McpServerError::HealthCheckFailed {
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
    pub async fn discover_resources(
        &mut self,
    ) -> Result<serde_json::Value, McpServerError> {
        if self.status != ServerStatus::Running {
            return Err(McpServerError::HealthCheckFailed {
                name: self.name.clone(),
                reason: "server not running".to_string(),
            });
        }

        let channel_opt = self.channel.take();
        let result = if let Some(ref channel) = channel_opt {
            let mut ch = channel.lock().await;
            ch.list_resources().await.map_err(|e| McpServerError::HealthCheckFailed {
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
        let mut channel = JsonRpcChannel::new(
            self.command.clone(),
            self.args.clone(),
            self.env.clone(),
        );

        channel.spawn().await.map_err(|e| {
            self.status = ServerStatus::Errored;
            McpServerError::Spawn {
                name: self.name.clone(),
                source: std::io::Error::new(std::io::ErrorKind::Other, e.clone()),
            }
        })?;

        let _ = channel.initialize().await;
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
