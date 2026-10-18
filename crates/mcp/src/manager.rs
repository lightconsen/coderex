//! MCP manager — coordinates servers, tools, resources, OAuth, and elicitation.

use crate::config::{McpConfig, McpTransport};
use crate::elicitation::ElicitationManager;
use crate::oauth::OAuthManager;
use crate::proxy::{McpToolInfo, McpToolProxy, McpToolRegistry};
use crate::resources::{McpResource, McpResourceContent, McpResourceRegistry};
use crate::server::{McpServer, ServerStatus};
use crate::tool::McpTool;
use coderex_tools::ToolRegistry;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Cached resource entry with TTL.
#[derive(Debug, Clone)]
struct CachedResource {
    content: McpResourceContent,
    cached_at: Instant,
}

/// Manager for all MCP servers, tools, and resources.
#[derive(Debug)]
pub struct McpManager {
    /// Server instances (shared for health monitor access).
    servers: Arc<DashMap<String, McpServer>>,
    /// Discovered tools.
    tools: Mutex<McpToolRegistry>,
    /// Discovered resources.
    resources: Mutex<McpResourceRegistry>,
    /// Resource read cache: (server_id, uri) -> content.
    resource_cache: Mutex<HashMap<(String, String), CachedResource>>,
    /// Default cache TTL.
    cache_ttl: Duration,
    /// OAuth state.
    ///
    /// Shared, because each remote server's token provider resolves the
    /// credential before every request and all of them must see the same
    /// refreshed state. Behind a lock for the same reason: a near-expiry
    /// refresh is a network round trip.
    oauth: Arc<tokio::sync::Mutex<OAuthManager>>,
    /// Elicitation state.
    elicitation: ElicitationManager,
    /// Whether a refresh (re-discovery) is needed.
    needs_refresh: AtomicBool,
    /// Health monitor cancellation sender.
    health_cancel: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
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
            servers: Arc::new(DashMap::new()),
            tools: Mutex::new(McpToolRegistry::new()),
            resources: Mutex::new(McpResourceRegistry::new()),
            resource_cache: Mutex::new(HashMap::new()),
            cache_ttl: Duration::from_secs(60),
            oauth: Arc::new(tokio::sync::Mutex::new(OAuthManager::new())),
            elicitation: ElicitationManager::new(),
            needs_refresh: AtomicBool::new(false),
            health_cancel: Mutex::new(None),
        }
    }

    /// Create a manager that starts with the tokens saved on disk.
    ///
    /// [`McpManager::new`] deliberately starts with no credentials so tests and
    /// short-lived callers never read the user's token file. This constructor
    /// is what the CLI uses: it picks up whatever the `McpAuth` tool persisted,
    /// so a server signed into in one session is authenticated in the next. A
    /// token file that cannot be read is logged and ignored — the manager comes
    /// up unauthenticated rather than refusing to start.
    #[must_use]
    pub fn new_with_stored_credentials() -> Self {
        let mut manager = Self::new();
        match OAuthManager::load_tokens() {
            Ok(stored) => {
                manager.oauth = Arc::new(tokio::sync::Mutex::new(stored));
            }
            Err(error) => {
                tracing::warn!("Could not read saved MCP tokens: {error}");
            }
        }
        manager
    }

    /// Load servers from a configuration.
    pub fn load_config(&self, config: &McpConfig) {
        for (name, server_config) in &config.mcp_servers {
            let server = match server_config.transport {
                McpTransport::Stdio => McpServer::new(
                    name.clone(),
                    server_config.command.clone(),
                    server_config.args.clone(),
                    server_config.env.clone(),
                ),
                McpTransport::Sse | McpTransport::Http => {
                    let Some(url) = server_config.url.clone() else {
                        tracing::warn!(
                            server = %name,
                            transport = ?server_config.transport,
                            "Skipping remote MCP server without a URL"
                        );
                        continue;
                    };
                    McpServer::new_http(
                        name.clone(),
                        url,
                        server_config.transport,
                        server_config.headers.clone(),
                    )
                }
            };
            self.servers.insert(name.clone(), server);
        }
        self.needs_refresh.store(true, Ordering::SeqCst);
    }

    /// Spawn all registered servers and discover tools/resources.
    ///
    /// Every remote server gets a token provider, which resolves the stored
    /// OAuth credential before *each* request rather than once here. That is
    /// what lets a session outlive its access token: the channel asks again on
    /// the next request, and a token inside the 5-minute expiry window is
    /// refreshed first. The provider degrades to "no credential" when the
    /// refresh fails, so the server can answer 401 instead of the request
    /// never being sent. Stdio servers are untouched: a subprocess is
    /// authenticated by its environment, not by an HTTP header.
    ///
    /// The client's timeout also bounds a token refresh, so a token endpoint
    /// that never answers cannot hold a request open indefinitely.
    pub async fn start_all(&self) {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let remote: Vec<(String, String)> = self
            .servers
            .iter()
            .filter_map(|entry| {
                entry
                    .value()
                    .url
                    .clone()
                    .map(|url| (entry.key().clone(), url))
            })
            .collect();

        for (name, url) in remote {
            let provider: Arc<dyn crate::http::TokenProvider> = Arc::new(
                crate::oauth::OAuthTokenProvider::new(Arc::clone(&self.oauth), url, client.clone()),
            );
            if let Some(mut server) = self.servers.get_mut(&name) {
                server.set_token_provider(provider);
            }
        }

        for mut entry in self.servers.iter_mut() {
            let server = entry.value_mut();
            let _ = server.spawn().await;
        }
        self.discover_all().await;
    }

    /// Discover tools and resources from all running servers.
    pub async fn discover_all(&self) {
        self.discover_all_tools().await;
        self.discover_all_resources().await;
    }

    /// Discover tools from all running servers.
    pub async fn discover_all_tools(&self) {
        for mut entry in self.servers.iter_mut() {
            let server = entry.value_mut();
            if server.status != crate::server::ServerStatus::Running {
                continue;
            }
            match server.discover_tools().await {
                Ok(value) => {
                    if let Some(tools) = value.get("tools").and_then(|t| t.as_array()) {
                        for tool in tools {
                            let name = tool
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let description = tool
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string();
                            let input_schema = tool
                                .get("inputSchema")
                                .cloned()
                                .unwrap_or_else(|| serde_json::json!({}));

                            self.register_tool(McpToolInfo {
                                server: server.name.clone(),
                                name,
                                description,
                                input_schema,
                            });
                        }
                        tracing::info!(
                            server = %server.name,
                            count = tools.len(),
                            "Discovered MCP tools"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(server = %server.name, "Tool discovery failed: {e}");
                }
            }
        }
    }

    /// Discover resources from all running servers.
    pub async fn discover_all_resources(&self) {
        for mut entry in self.servers.iter_mut() {
            let server = entry.value_mut();
            if server.status != crate::server::ServerStatus::Running {
                continue;
            }
            match server.discover_resources().await {
                Ok(value) => {
                    if let Some(resources) = value.get("resources").and_then(|r| r.as_array()) {
                        for res in resources {
                            let uri = res
                                .get("uri")
                                .and_then(|u| u.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = res
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let mime_type = res
                                .get("mimeType")
                                .and_then(|m| m.as_str())
                                .unwrap_or("application/octet-stream")
                                .to_string();
                            let description = res
                                .get("description")
                                .and_then(|d| d.as_str())
                                .map(String::from);

                            self.register_resource(crate::resources::McpResource {
                                uri,
                                name,
                                mime_type,
                                description,
                            });
                        }
                        tracing::info!(
                            server = %server.name,
                            count = resources.len(),
                            "Discovered MCP resources"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(server = %server.name, "Resource discovery failed: {e}");
                }
            }
        }
    }

    /// Stop all servers and cancel health monitor.
    pub async fn stop_all(&self) {
        self.cancel_health_monitor();
        for mut entry in self.servers.iter_mut() {
            let server = entry.value_mut();
            server.stop().await;
        }
    }

    /// Spawn a background health monitor that checks all servers every 30s.
    pub fn spawn_health_monitor(&self) {
        self.cancel_health_monitor();

        let (tx, mut rx) = tokio::sync::watch::channel(false);
        *self.health_cancel.lock().unwrap() = Some(tx);

        let servers = Arc::clone(&self.servers);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));

            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = rx.changed() => {
                        if *rx.borrow() {
                            break;
                        }
                    }
                }

                for entry in servers.iter() {
                    let name = entry.key().clone();
                    let server = entry.value();
                    if server.status != ServerStatus::Running {
                        continue;
                    }
                    if let Err(e) = server.health_check().await {
                        tracing::warn!(server = %name, "MCP health check failed: {e}");
                    }
                }
            }

            tracing::debug!("MCP health monitor stopped");
        });
    }

    /// Cancel the health monitor background task.
    fn cancel_health_monitor(&self) {
        if let Some(tx) = self.health_cancel.lock().unwrap().take() {
            let _ = tx.send(true);
        }
    }

    /// Get a server by name.
    #[must_use]
    pub fn get_server(&self, name: &str) -> Option<McpServer> {
        self.servers.get(name).map(|entry| entry.value().clone())
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

    /// Register every discovered tool into `registry`.
    ///
    /// This is what puts MCP tools on the same execution path as built-ins:
    /// the registry's dispatcher applies its timeouts, output limits, and
    /// concurrency rules to them like any other tool. Safe to call before the
    /// servers are up — each tool resolves its connection when it is called.
    pub fn register_tools_into(self: &Arc<Self>, registry: &ToolRegistry) {
        for info in self.list_tool_info() {
            registry.register_dynamic(Arc::new(McpTool::new(info, Arc::clone(self))));
        }
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
            .map(|t| t.qualified_name())
            .collect()
    }

    /// List all registered tools with full metadata.
    #[must_use]
    pub fn list_tool_info(&self) -> Vec<McpToolInfo> {
        self.tools
            .lock()
            .unwrap()
            .list()
            .iter()
            .map(|t| (*t).clone())
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

    /// Get a cached resource if present and not expired.
    #[must_use]
    pub fn get_cached_resource(&self, server_id: &str, uri: &str) -> Option<McpResourceContent> {
        let cache = self.resource_cache.lock().unwrap();
        let key = (server_id.to_string(), uri.to_string());
        cache.get(&key).and_then(|entry| {
            if entry.cached_at.elapsed() < self.cache_ttl {
                Some(entry.content.clone())
            } else {
                None
            }
        })
    }

    /// Store a resource in the cache.
    pub fn cache_resource(&self, server_id: &str, uri: &str, content: McpResourceContent) {
        let mut cache = self.resource_cache.lock().unwrap();
        cache.insert(
            (server_id.to_string(), uri.to_string()),
            CachedResource {
                content,
                cached_at: Instant::now(),
            },
        );
    }

    /// Invalidate a specific cached resource.
    pub fn invalidate_resource_cache(&self, server_id: &str, uri: &str) {
        let mut cache = self.resource_cache.lock().unwrap();
        cache.remove(&(server_id.to_string(), uri.to_string()));
    }

    /// Clear the entire resource cache.
    pub fn clear_resource_cache(&self) {
        let mut cache = self.resource_cache.lock().unwrap();
        cache.clear();
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
    pub async fn oauth(&self) -> tokio::sync::MutexGuard<'_, OAuthManager> {
        self.oauth.lock().await
    }

    /// Mutable access to the OAuth manager, without waiting on the lock.
    ///
    /// Needs `&mut self`, which is what makes it usable from setup code — the
    /// running manager hands out [`McpManager::oauth`] instead.
    #[must_use]
    pub fn oauth_mut(&mut self) -> &mut OAuthManager {
        Arc::get_mut(&mut self.oauth)
            .expect("the OAuth manager has another strong owner")
            .get_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpServerConfig, McpTransport};
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
        config.add_server(
            "fs",
            McpServerConfig {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
                ..Default::default()
            },
        );
        mgr.load_config(&config);
        assert_eq!(mgr.list_servers().len(), 1);
        assert!(mgr.needs_refresh());
    }

    #[tokio::test]
    async fn start_and_stop() {
        let mgr = McpManager::new();
        let mut config = McpConfig::new();
        config.add_server(
            "test",
            McpServerConfig {
                command: "cat".into(),
                args: vec![],
                env: HashMap::new(),
                ..Default::default()
            },
        );
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
        config.add_server(
            "fs",
            McpServerConfig {
                command: "npx".into(),
                args: vec![],
                env: HashMap::new(),
                ..Default::default()
            },
        );
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
        assert!(mgr.list_tools().contains(&"mcp__fs__read_file".to_string()));
        assert!(
            mgr.list_resources()
                .contains(&"file:///tmp/test.txt".to_string())
        );
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
        assert!(mgr.get_tool_proxy("mcp__fs__read_file").is_none());
    }

    #[tokio::test]
    async fn start_all_installs_a_token_provider_for_remote_servers() {
        // Port 1 refuses connections: connecting is not what is being checked.
        // The provider is installed before the connection is attempted, and it
        // is what resolves the token — at request time, not startup time.
        let url = "http://127.0.0.1:1/mcp";
        let mut mgr = McpManager::new();
        let mut state = crate::oauth::McpOAuthState::new(url, "client1");
        state.update_tokens("token_abc".into(), None, 3600);
        mgr.oauth_mut().register(state);

        let mut config = McpConfig::new();
        config.add_server(
            "remote",
            McpServerConfig {
                transport: McpTransport::Http,
                url: Some(url.into()),
                ..Default::default()
            },
        );
        mgr.load_config(&config);
        mgr.start_all().await;

        let server = mgr.get_server("remote").unwrap();
        assert!(
            !server.headers.contains_key("Authorization"),
            "the credential must not be frozen into a startup header"
        );
        let provider = server
            .token_provider()
            .expect("a remote server needs a token provider");
        assert_eq!(provider.access_token().await.as_deref(), Some("token_abc"));
    }

    #[tokio::test]
    async fn start_all_leaves_a_remote_server_without_a_token_alone() {
        let url = "http://127.0.0.1:1/mcp";
        let mgr = McpManager::new();

        let mut config = McpConfig::new();
        config.add_server(
            "remote",
            McpServerConfig {
                transport: McpTransport::Http,
                url: Some(url.into()),
                ..Default::default()
            },
        );
        mgr.load_config(&config);
        mgr.start_all().await;

        let server = mgr.get_server("remote").unwrap();
        assert!(!server.headers.contains_key("Authorization"));
        let provider = server
            .token_provider()
            .expect("a remote server needs a token provider");
        assert!(provider.access_token().await.is_none());
    }
}
