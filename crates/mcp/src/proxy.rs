//! MCP tool proxy — tool discovery, registration, and call forwarding.

use crate::channel::JsonRpcChannel;
use crate::server::McpServer;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Information about a discovered MCP tool.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    /// The server that provides this tool.
    pub server: String,
    /// The tool name.
    pub name: String,
    /// Description of what the tool does.
    pub description: String,
    /// JSON schema for the tool input.
    pub input_schema: Value,
}

/// Proxy for calling an MCP tool on a remote server.
#[derive(Debug)]
pub struct McpToolProxy {
    /// Server name this proxy connects to.
    pub server: String,
    /// Tool name on the server.
    pub tool: String,
    /// Shared JSON-RPC channel to the server.
    channel: Option<Arc<Mutex<JsonRpcChannel>>>,
}

impl McpToolProxy {
    /// Create a new tool proxy (disconnected).
    #[must_use]
    pub fn new(server: impl Into<String>, tool: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            tool: tool.into(),
            channel: None,
        }
    }

    /// Create a new tool proxy with a connected channel.
    #[must_use]
    pub fn with_channel(
        server: impl Into<String>,
        tool: impl Into<String>,
        channel: Arc<Mutex<JsonRpcChannel>>,
    ) -> Self {
        Self {
            server: server.into(),
            tool: tool.into(),
            channel: Some(channel),
        }
    }

    /// Bind this proxy to a server's channel.
    pub fn bind_to_server(&mut self, server: &McpServer) {
        if let Some(ch) = server.channel() {
            self.channel = Some(ch);
        }
    }

    /// Execute the tool with the given arguments.
    pub async fn call(&self, args: Value) -> Result<Value, String> {
        let channel = self
            .channel
            .as_ref()
            .ok_or_else(|| format!("MCP server '{}' not connected", self.server))?;

        let mut ch = channel.lock().await;

        // MCP tools/call format
        let params = serde_json::json!({
            "name": self.tool,
            "arguments": args
        });

        let result = ch.send_request("tools/call", Some(params)).await?;
        Ok(result)
    }
}

/// Tool registry for MCP-discovered tools.
#[derive(Debug, Default)]
pub struct McpToolRegistry {
    tools: HashMap<String, McpToolInfo>,
}

impl McpToolRegistry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a discovered tool.
    pub fn register(&mut self, info: McpToolInfo) {
        self.tools.insert(info.name.clone(), info);
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&McpToolInfo> {
        self.tools.get(name)
    }

    /// Remove a tool.
    pub fn unregister(&mut self, name: &str) -> Option<McpToolInfo> {
        self.tools.remove(name)
    }

    /// List all registered tools.
    #[must_use]
    pub fn list(&self) -> Vec<&McpToolInfo> {
        self.tools.values().collect()
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Check if no tools are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tool() -> McpToolInfo {
        McpToolInfo {
            server: "filesystem".into(),
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        }
    }

    #[test]
    fn registry_starts_empty() {
        let reg = McpToolRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn register_and_get() {
        let mut reg = McpToolRegistry::new();
        let tool = sample_tool();
        reg.register(tool);
        assert_eq!(reg.len(), 1);
        let got = reg.get("read_file").unwrap();
        assert_eq!(got.server, "filesystem");
        assert_eq!(got.description, "Read a file");
    }

    #[test]
    fn unregister() {
        let mut reg = McpToolRegistry::new();
        reg.register(sample_tool());
        let removed = reg.unregister("read_file");
        assert!(removed.is_some());
        assert!(reg.is_empty());
    }

    #[test]
    fn proxy_call_errors_when_disconnected() {
        let proxy = McpToolProxy::new("filesystem", "read_file");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(proxy.call(serde_json::json!({ "path": "/tmp/test" })));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not connected"));
    }

    #[test]
    fn proxy_new() {
        let proxy = McpToolProxy::new("server1", "tool1");
        assert_eq!(proxy.server, "server1");
        assert_eq!(proxy.tool, "tool1");
        assert!(proxy.channel.is_none());
    }

    #[test]
    fn proxy_with_server_bind() {
        let mut proxy = McpToolProxy::new("test", "echo");
        let server = McpServer::new("test", "cat", vec![], std::collections::HashMap::new());
        // Server not spawned, so channel is None — bind is safe no-op
        proxy.bind_to_server(&server);
        assert!(proxy.channel.is_none());
    }
}
