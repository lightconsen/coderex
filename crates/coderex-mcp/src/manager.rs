//! MCP manager — coordinates servers, tools, resources, OAuth, and elicitation.

use crate::config::McpConfig;
use crate::elicitation::ElicitationManager;
use crate::oauth::OAuthManager;
use crate::proxy::{McpToolInfo, McpToolProxy, McpToolRegistry};
use crate::resources::{McpResource, McpResourceRegistry};
use crate::server::{McpServer, ServerStatus};
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Manager for all MCP servers, tools, and resources.
#[derive(Debug)]
pub struct McpManager {
    /// Server instances.
    servers: DashMap<String, McpServer>,
    /// Discovered tools.
    tools: Mutex<McpToolRegistry>,
    /// Discovered resources.
    resources: Mutex<McpResourceRegistry>,
    /// OAuth state.
    oauth: OAuthManager,
    /// Elicitation state.
    elicitation: ElicitationManager,
    /// Whether a refresh (re-discovery) is needed.
    needs_refresh: AtomicBool,
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
            tools: Mutex::new(McpToolRegistry::new()),
            resources: Mutex::new(McpResourceRegistry::new()),
            oauth: OAuthManager::new(),
            elicitation: ElicitationManager::new(),
            needs_refresh: AtomicBool::new(false),
        }
    }

    /// Load servers from a configuration.
    pub fn load_config(&self, config: &McpConfig) {
        for (name, server_config) in &config.mcp_servers {
            let server = McpServer::new(
                name.clone(),
                server_config.command.clone(),
                server_config.args.clone(),
                server_config.env.clone(),
            );
            self.servers.insert(name.clone(), server);
        }
        self.needs_refresh.store(true, Ordering::SeqCst);
    }

    /// Spawn all registered servers.
    pub async fn start_all(&self) {
        for mut entry in self.servers.iter_mut() {
            let server = entry.value_mut();
            let _ = server.spawn().await;
        }
    }

    /// Stop all servers.
    pub async fn stop_all(&self) {
        for mut entry in self.servers.iter_mut() {
            let server = entry.value_mut();
            server.stop().await;
        }
    }

    /// Get a server by name.
    #[must_use]
    pub fn get_server(&self, name: &str) -> Option<McpServer> {
        self.servers.get(name).map(|e| {
            let s = e.value();
            McpServer::new(&s.name, &s.command, s.args.clone(), s.env.clone())
        })
    }

    /// List server names.
    #[must_use]
    pub fn list_servers(&self) -> Vec<String> {
        self.servers.iter().map(|e| e.key().clone()).collect()
    }

    /// Check if a server is running.
    #[must_use]
    pub fn is_running(&self, name: &str) -> bool {
        self.servers
            .get(name)
            .map(|e| e.value().status == ServerStatus::Running)
            .unwrap_or(false)
    }

    /// Register a discovered tool in the registry.
    pub fn register_tool(&self, info: McpToolInfo) {
        self.tools.lock().unwrap().register(info);
    }

    /// Register a discovered resource in the registry.
    pub fn register_resource(&self, resource: McpResource) {
        self.resources.lock().unwrap().register(resource);
    }

    /// Get a tool proxy for executing an MCP tool (if server is running).
    pub fn get_tool_proxy(&self, tool_name: &str) -> Option<McpToolProxy> {
        let (server_name, tool_name_owned) = {
            let tools = self.tools.lock().unwrap();
            let info = tools.get(tool_name)?;
            (info.server.clone(), info.name.clone())
        };

        let server_entry = self.servers.get(&server_name)?;
        let server = server_entry.value();

        if server.status != ServerStatus::Running {
            return None;
        }

        let channel = server.channel()?;
        Some(McpToolProxy::with_channel(
            &server_name,
            &tool_name_owned,
            channel,
        ))
    }

    /// List all registered tool names.
    #[must_use]
    pub fn list_tools(&self) -> Vec<String> {
        self.tools
            .lock()
            .unwrap()
            .list()
            .iter()
            .map(|t| t.name.clone())
            .collect()
    }

    /// List all registered resource URIs.
    #[must_use]
    pub fn list_resources(&self) -> Vec<String> {
        self.resources
            .lock()
            .unwrap()
            .list()
            .iter()
            .map(|r| r.uri.clone())
            .collect()
    }

    /// Number of registered tools.
    #[must_use]
    pub fn tool_count(&self) -> usize {
        self.tools.lock().unwrap().len()
    }

    /// Number of registered resources.
    #[must_use]
    pub fn resource_count(&self) -> usize {
        self.resources.lock().unwrap().len()
    }

    /// Mark as needing refresh.
    pub fn mark_needs_refresh(&self) {
        self.needs_refresh.store(true, Ordering::SeqCst);
    }

    /// Clear the refresh flag.
    pub fn clear_refresh_flag(&self) {
        self.needs_refresh.store(false, Ordering::SeqCst);
    }

    /// Check if a refresh is needed.
    #[must_use]
    pub fn needs_refresh(&self) -> bool {
        self.needs_refresh.load(Ordering::Relaxed)
    }

    /// Access the elicitation manager.
    #[must_use]
    pub fn elicitation(&self) -> &ElicitationManager {
        &self.elicitation
    }

    /// Mutable access to the elicitation manager.
    #[must_use]
    pub fn elicitation_mut(&mut self) -> &mut ElicitationManager {
        &mut self.elicitation
    }

    /// Access the OAuth manager.
    #[must_use]
    pub fn oauth(&self) -> &OAuthManager {
        &self.oauth
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConfig;
    use std::collections::HashMap;

    #[test]
    fn new_manager_is_empty() {
        let mgr = McpManager::new();
        assert!(mgr.list_servers().is_empty());
        assert!(!mgr.needs_refresh());
    }

    #[test]
    fn load_config_adds_servers() {
        let mgr = McpManager::new();
        let mut config = McpConfig::new();
        config.add_server("fs", McpServerConfig {
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
        });
        mgr.load_config(&config);
        assert_eq!(mgr.list_servers().len(), 1);
        assert!(mgr.needs_refresh());
    }

    #[tokio::test]
    async fn start_and_stop() {
        let mgr = McpManager::new();
        let mut config = McpConfig::new();
        config.add_server("test", McpServerConfig {
            command: "cat".into(),
            args: vec![],
            env: HashMap::new(),
        });
        mgr.load_config(&config);

        mgr.start_all().await;
        assert!(mgr.is_running("test"));

        mgr.stop_all().await;
        assert!(!mgr.is_running("test"));
    }

    #[test]
    fn get_server() {
        let mgr = McpManager::new();
        let mut config = McpConfig::new();
        config.add_server("fs", McpServerConfig {
            command: "npx".into(),
            args: vec![],
            env: HashMap::new(),
        });
        mgr.load_config(&config);

        let srv = mgr.get_server("fs").unwrap();
        assert_eq!(srv.command, "npx");
        assert!(mgr.get_server("nope").is_none());
    }

    #[test]
    fn refresh_flag() {
        let mgr = McpManager::new();
        assert!(!mgr.needs_refresh());
        mgr.mark_needs_refresh();
        assert!(mgr.needs_refresh());
        mgr.clear_refresh_flag();
        assert!(!mgr.needs_refresh());
    }

    #[test]
    fn register_tool_and_resource() {
        let mgr = McpManager::new();

        mgr.register_tool(McpToolInfo {
            server: "fs".into(),
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({}),
        });

        mgr.register_resource(McpResource {
            uri: "file:///tmp/test.txt".into(),
            name: "test.txt".into(),
            mime_type: "text/plain".into(),
            description: Some("A test file".into()),
        });

        assert_eq!(mgr.tool_count(), 1);
        assert_eq!(mgr.resource_count(), 1);
        assert!(mgr.list_tools().contains(&"read_file".to_string()));
        assert!(mgr.list_resources().contains(&"file:///tmp/test.txt".to_string()));
    }

    #[test]
    fn get_tool_proxy_when_server_not_running() {
        let mgr = McpManager::new();

        mgr.register_tool(McpToolInfo {
            server: "fs".into(),
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({}),
        });

        // Server not started, so proxy should be None
        assert!(mgr.get_tool_proxy("read_file").is_none());
    }
}
