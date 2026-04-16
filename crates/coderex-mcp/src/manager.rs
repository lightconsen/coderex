//! MCP manager — coordinates multiple MCP servers.

use dashmap::DashMap;
use crate::server::McpServer;

/// Information about a discovered MCP tool.
#[derive(Debug)]
#[allow(dead_code)]
pub struct McpToolInfo {
    /// The server that provides this tool.
    pub server: String,
    /// The tool name.
    pub name: String,
    /// The JSON schema for the tool's input.
    pub input_schema: serde_json::Value,
}

/// Information about a discovered MCP resource.
#[derive(Debug)]
#[allow(dead_code)]
pub struct McpResourceInfo {
    /// The server that provides this resource.
    pub server: String,
    /// The resource URI.
    pub uri: String,
    /// The resource name.
    pub name: String,
}

/// Manager for all MCP servers, tools, and resources.
#[derive(Debug)]
#[allow(dead_code)]
pub struct McpManager {
    servers: DashMap<String, McpServer>,
    tools: DashMap<String, McpToolInfo>,
    resources: DashMap<String, McpResourceInfo>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    /// Create a new MCP manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            servers: DashMap::new(),
            tools: DashMap::new(),
            resources: DashMap::new(),
        }
    }
}
