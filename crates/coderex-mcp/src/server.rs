//! MCP server lifecycle — stdio JSON-RPC server spawn and management.

use thiserror::Error;

/// Errors from MCP server operations.
#[derive(Debug, Error)]
pub enum McpServerError {
    /// Failed to spawn the MCP server process.
    #[error("failed to spawn server: {0}")]
    Spawn(#[from] std::io::Error),
    /// The named MCP server was not found.
    #[error("server '{name}' not found")]
    NotFound {
        /// The name of the server that was not found.
        name: String,
    },
}

/// A running MCP server instance.
#[derive(Debug)]
#[allow(dead_code)]
pub struct McpServer {
    /// Server name/identifier.
    pub name: String,
}

impl McpServer {
    /// Spawn a new MCP server from configuration.
    #[allow(clippy::unused_async)]
    pub async fn spawn(_command: &str, _args: &[&str]) -> Result<Self, McpServerError> {
        // TODO: stdio JSON-RPC server lifecycle
        Ok(Self {
            name: String::new(),
        })
    }
}
