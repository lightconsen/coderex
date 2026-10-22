//! MCP manager — coordinates servers, tools, resources, prompts, OAuth, and
//! elicitation.

use crate::config::{McpConfig, McpTransport};
use crate::elicitation::ElicitationManager;
use crate::oauth::OAuthManager;
use crate::prompts::{
    McpPromptInfo, McpPromptRegistry, McpPromptResult, ServerPrompt, parse_prompt_list,
    parse_prompt_result,
};
use crate::proxy::{McpToolInfo, McpToolProxy, McpToolRegistry};
use crate::resources::{McpResource, McpResourceContent, McpResourceRegistry};
use crate::server::{McpServer, ServerStatus};
use crate::tool::McpTool;
use coderex_tools::ToolRegistry;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// A consumer outside this crate that wants every server notification.
///
/// The MCP crate has no dependency on the plugin crate, so the layer that
/// turns a notification into a plugin channel delivery cannot be named here.
/// The CLI installs a router instead: [`McpManager::set_notification_router`]
/// records it, and the per-server notification handler offers it every
/// notification a configured server emits — the same ones the manager already
/// acts on internally (`tools/list_changed`, `resources/updated`,
/// `prompts/list_changed`) and every other method too.
///
/// The call is synchronous and must not block: it runs on the stream task that
/// received the notification.
pub trait ServerNotificationRouter: Send + Sync + std::fmt::Debug {
    /// Route one notification. `server` is the configured server name, `method`
    /// the JSON-RPC method, and `params` the notification parameters as sent.
    fn route(&self, server: &str, method: &str, params: Option<&serde_json::Value>);
}

/// The router as installed, shared so the manager can swap it without touching
/// the per-server handlers already handed out.
type SharedNotificationRouter = Arc<RwLock<Option<Arc<dyn ServerNotificationRouter>>>>;

/// Cached resource entry with TTL.
#[derive(Debug, Clone)]
struct CachedResource {
    content: McpResourceContent,
    cached_at: Instant,
}

/// The state a notification handler needs to act without borrowing the
/// manager: a server pushes `tools/list_changed` from a stream task, which
/// cannot name the manager that owns it.
#[derive(Debug, Clone)]
struct NotificationSink {
    servers: Arc<DashMap<String, McpServer>>,
    tools: Arc<Mutex<McpToolRegistry>>,
    prompts: Arc<Mutex<McpPromptRegistry>>,
    resource_cache: Arc<Mutex<HashMap<(String, String), CachedResource>>>,
    /// Offered every notification, including the methods acted on above.
    router: SharedNotificationRouter,
}

impl NotificationSink {
    /// Re-run `tools/list` for one server and replace that server's tools.
    ///
    /// The old list is dropped only after the new one arrives, and only if the
    /// server answered: a transient failure must not empty the registry.
    async fn refresh_tools(&self, server_name: &str) {
        // Clone the server and drop the shard guard before awaiting: this
        // handler runs on the stream's task, and a synchronous guard held
        // across the request would deadlock the manager's discovery on a
        // current-thread runtime.
        let Some(mut server) = self
            .servers
            .get(server_name)
            .map(|entry| entry.value().clone())
        else {
            tracing::debug!(
                server = server_name,
                "tools/list_changed for a server that is no longer registered"
            );
            return;
        };
        if server.status != ServerStatus::Running {
            return;
        }

        let value = match server.discover_tools().await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    server = server_name,
                    "re-discovering tools after tools/list_changed failed: {error}"
                );
                return;
            }
        };
        let Some(tools) = value.get("tools").and_then(|tools| tools.as_array()) else {
            return;
        };

        let mut registry = self
            .tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = registry.unregister_server(server_name);
        for tool in tools {
            let name = tool
                .get("name")
                .and_then(|name| name.as_str())
                .unwrap_or("unknown")
                .to_string();
            let description = tool
                .get("description")
                .and_then(|description| description.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            registry.register(McpToolInfo {
                server: server_name.to_string(),
                name,
                description,
                input_schema,
            });
        }
        drop(registry);
        tracing::info!(
            server = server_name,
            count = tools.len(),
            removed,
            "Re-discovered MCP tools after tools/list_changed"
        );
    }

    /// Re-run `prompts/list` for one server and replace that server's prompts.
    ///
    /// A failure leaves the previous list in place: a transient error must not
    /// make the prompts disappear.
    async fn refresh_prompts(&self, server_name: &str) {
        let Some(mut server) = self
            .servers
            .get(server_name)
            .map(|entry| entry.value().clone())
        else {
            tracing::debug!(
                server = server_name,
                "prompts/list_changed for a server that is no longer registered"
            );
            return;
        };
        if server.status != ServerStatus::Running {
            return;
        }

        let response = match server.discover_prompts().await {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(
                    server = server_name,
                    "re-discovering prompts after prompts/list_changed failed: {error}"
                );
                return;
            }
        };
        let prompts = match parse_prompt_list(&response) {
            Ok(prompts) => prompts,
            Err(error) => {
                tracing::warn!(
                    server = server_name,
                    "prompts/list_changed returned an invalid prompt list: {error}"
                );
                return;
            }
        };

        let count = prompts.len();
        let mut registry = self
            .prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.replace_server(server_name, prompts);
        drop(registry);
        tracing::info!(
            server = server_name,
            count,
            "Re-discovered MCP prompts after prompts/list_changed"
        );
    }

    /// Drop cached content for one resource, or every resource of a server
    /// when the notification names none.
    fn invalidate_resource(&self, server_name: &str, uri: Option<&str>) {
        let mut cache = self
            .resource_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(uri) = uri {
            cache.remove(&(server_name.to_string(), uri.to_string()));
            tracing::debug!(server = server_name, uri, "invalidated a cached resource");
        } else {
            cache.retain(|(server, _), _| server != server_name);
            tracing::debug!(
                server = server_name,
                "invalidated every cached resource for the server"
            );
        }
        drop(cache);
    }

    /// Offer one notification to the installed router, if any.
    ///
    /// The router is cloned out of the lock before it is called so a router
    /// that swaps itself out cannot deadlock the manager.
    fn route(&self, server: &str, method: &str, params: Option<&serde_json::Value>) {
        let router = self
            .router
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(router) = router {
            router.route(server, method, params);
        }
    }
}

/// The notification sink as installed on one server.
#[derive(Debug)]
struct PerServerNotificationHandler {
    server: String,
    sink: NotificationSink,
}

#[async_trait::async_trait]
impl crate::channel::ServerNotificationHandler for PerServerNotificationHandler {
    async fn handle_notification(&self, method: &str, params: Option<&serde_json::Value>) {
        match method {
            "notifications/tools/list_changed" => {
                self.sink.refresh_tools(&self.server).await;
            }
            "notifications/resources/updated" => {
                let uri = params
                    .and_then(|params| params.get("uri"))
                    .and_then(serde_json::Value::as_str);
                self.sink.invalidate_resource(&self.server, uri);
            }
            "notifications/prompts/list_changed" => {
                self.sink.refresh_prompts(&self.server).await;
            }
            other => tracing::debug!(
                server = %self.server,
                method = other,
                "server notification"
            ),
        }
        // Every notification is also offered to the installed router. The two
        // methods handled above are the manager's own bookkeeping; a plugin
        // channel bound to this server may care about any method, so the router
        // sees all of them rather than only the two this crate understands.
        self.sink.route(&self.server, method, params);
    }
}

/// Manager for all MCP servers, tools, and resources.
#[derive(Debug)]
pub struct McpManager {
    /// Server instances (shared for health monitor access).
    servers: Arc<DashMap<String, McpServer>>,
    /// Discovered tools.
    ///
    /// Shared (like the resource state) because the per-server notification
    /// handler re-discovers and updates them from the stream's task, where the
    /// manager itself is not borrowed.
    tools: Arc<Mutex<McpToolRegistry>>,
    /// Discovered resources.
    resources: Arc<Mutex<McpResourceRegistry>>,
    /// Discovered prompts.
    ///
    /// Shared for the same reason as the tool and resource registries: a
    /// `prompts/list_changed` notification re-discovers them from the stream
    /// task, where the manager itself is not borrowed.
    prompts: Arc<Mutex<McpPromptRegistry>>,
    /// Resource read cache: (server_id, uri) -> content.
    resource_cache: Arc<Mutex<HashMap<(String, String), CachedResource>>>,
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
    /// Answers requests the servers make of the client.
    ///
    /// One slot shared by every server, so the session can install its UI
    /// handler at any time — including after `start_all` — and every running
    /// channel sees it.
    inbound_handler: crate::channel::SharedInboundHandler,
    /// Whether a refresh (re-discovery) is needed.
    needs_refresh: Arc<AtomicBool>,
    /// Health monitor cancellation sender.
    health_cancel: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
    /// Offered every server notification, for a consumer outside this crate
    /// (the CLI installs one that routes into the plugin channel layer).
    ///
    /// Shared behind a lock so it can be installed after servers are created
    /// and swapped at runtime: every per-server handler holds the same handle.
    notification_router: SharedNotificationRouter,
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
            tools: Arc::new(Mutex::new(McpToolRegistry::new())),
            resources: Arc::new(Mutex::new(McpResourceRegistry::new())),
            prompts: Arc::new(Mutex::new(McpPromptRegistry::new())),
            resource_cache: Arc::new(Mutex::new(HashMap::new())),
            cache_ttl: Duration::from_secs(60),
            oauth: Arc::new(tokio::sync::Mutex::new(OAuthManager::new())),
            elicitation: ElicitationManager::new(),
            inbound_handler: crate::channel::SharedInboundHandler::new(),
            needs_refresh: Arc::new(AtomicBool::new(false)),
            health_cancel: Mutex::new(None),
            notification_router: Arc::new(RwLock::new(None)),
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

    /// The state a per-server notification handler works from.
    fn notification_sink(&self) -> NotificationSink {
        NotificationSink {
            servers: Arc::clone(&self.servers),
            tools: Arc::clone(&self.tools),
            prompts: Arc::clone(&self.prompts),
            resource_cache: Arc::clone(&self.resource_cache),
            router: Arc::clone(&self.notification_router),
        }
    }

    /// Load servers from a configuration.
    pub fn load_config(&self, config: &McpConfig) {
        let sink = self.notification_sink();
        for (name, server_config) in &config.mcp_servers {
            let mut server = match server_config.transport {
                McpTransport::Stdio => {
                    if let Err(error) = server_config.sandbox.validate() {
                        tracing::warn!(server = %name, %error, "Skipping MCP server");
                        continue;
                    }
                    let mut server = McpServer::new(
                        name.clone(),
                        server_config.command.clone(),
                        server_config.args.clone(),
                        server_config.env.clone(),
                    );
                    server.cwd.clone_from(&server_config.cwd);
                    server.sandbox = server_config.sandbox.clone();
                    server
                }
                // Both remote transports keep their own identity here: the
                // channel branches on the transport, so `Sse` gets the classic
                // GET-stream + POST-endpoint flow while `Http` gets streamable
                // HTTP. Neither is an alias for the other.
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
            server.share_inbound_handler(self.inbound_handler.clone());
            // Re-discovery and cache invalidation must work from the stream
            // task, so each server is told which server it is.
            server.set_notification_handler(Arc::new(PerServerNotificationHandler {
                server: name.clone(),
                sink: sink.clone(),
            }));
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
                crate::oauth::OAuthTokenProvider::new(&self.oauth, url, client.clone()),
            );
            if let Some(mut server) = self.servers.get_mut(&name) {
                server.set_token_provider(provider);
            }
        }

        // Take each server out while it starts rather than holding a DashMap
        // guard across `spawn().await`: a synchronous guard held across an
        // await deadlocks anything else that reaches for the same entry on a
        // current-thread runtime (the standalone stream's notification
        // handler is one such caller).
        let names: Vec<String> = self.servers.iter().map(|e| e.key().clone()).collect();
        for name in names {
            let Some((_, mut server)) = self.servers.remove(&name) else {
                continue;
            };
            let _ = server.spawn().await;
            self.servers.insert(name, server);
        }
        self.discover_all().await;
    }

    /// Discover tools, resources, and prompts from all running servers.
    pub async fn discover_all(&self) {
        self.discover_all_tools().await;
        self.discover_all_resources().await;
        self.discover_all_prompts().await;
    }

    /// Discover tools from all running servers.
    pub async fn discover_all_tools(&self) {
        // `McpServer` is deliberately cloneable and its channel is shared, so
        // discovery can run on a clone after the map lock is released. Holding
        // a shard guard across the request would deadlock a concurrent
        // notification handler.
        let servers: Vec<McpServer> = self
            .servers
            .iter()
            .filter(|entry| entry.value().status == crate::server::ServerStatus::Running)
            .map(|entry| entry.value().clone())
            .collect();
        for mut server in servers {
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
        let servers: Vec<McpServer> = self
            .servers
            .iter()
            .filter(|entry| entry.value().status == crate::server::ServerStatus::Running)
            .map(|entry| entry.value().clone())
            .collect();
        for mut server in servers {
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

    /// Discover prompts from all running servers.
    ///
    /// Each server is queried independently: one that errors is logged under
    /// its own name and leaves its previously discovered prompts untouched,
    /// while the other servers are still discovered. A server with no prompts
    /// is simply absent from the aggregate.
    pub async fn discover_all_prompts(&self) {
        let servers: Vec<McpServer> = self
            .servers
            .iter()
            .filter(|entry| entry.value().status == crate::server::ServerStatus::Running)
            .map(|entry| entry.value().clone())
            .collect();
        for mut server in servers {
            match server.discover_prompts().await {
                Ok(value) => match parse_prompt_list(&value) {
                    Ok(prompts) => {
                        let count = prompts.len();
                        self.prompts
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .replace_server(&server.name, prompts);
                        tracing::info!(
                            server = %server.name,
                            count,
                            "Discovered MCP prompts"
                        );
                    }
                    Err(error) => tracing::warn!(
                        server = %server.name,
                        "Prompt discovery returned an invalid response: {error}"
                    ),
                },
                Err(error) => {
                    tracing::warn!(server = %server.name, "Prompt discovery failed: {error}");
                }
            }
        }
    }

    /// Stop all servers and cancel health monitor.
    pub async fn stop_all(&self) {
        self.cancel_health_monitor();
        let names: Vec<String> = self.servers.iter().map(|e| e.key().clone()).collect();
        for name in names {
            let Some((_, mut server)) = self.servers.remove(&name) else {
                continue;
            };
            server.stop().await;
            self.servers.insert(name, server);
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

                let running: Vec<(String, McpServer)> = servers
                    .iter()
                    .filter(|entry| entry.value().status == ServerStatus::Running)
                    .map(|entry| (entry.key().clone(), entry.value().clone()))
                    .collect();
                for (name, server) in running {
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
        let cancel = self.health_cancel.lock().unwrap().take();
        if let Some(tx) = cancel {
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
            .is_some_and(|e| e.value().status == ServerStatus::Running)
    }

    /// Register a discovered tool in the registry.
    pub fn register_tool(&self, info: McpToolInfo) {
        self.tools.lock().unwrap().register(info);
    }

    /// Public tool names that could not be registered because another server
    /// already exposed the same name.
    ///
    /// This can only happen when the `mcp__<server>__<tool>` prefix is
    /// disabled (see [`crate::proxy::set_mcp_prefix_enabled`]); in the default
    /// prefixed mode every name is unique and the result is empty. Names are
    /// sorted and deduplicated so the list is stable between calls.
    ///
    /// The CLI can surface this — for example from `/mcp` — to tell the user
    /// that a tool exists but is unreachable without turning the prefix back
    /// on or renaming it on its server.
    #[must_use]
    pub fn tool_conflicts(&self) -> Vec<String> {
        let registry = self
            .tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut names: Vec<String> = registry
            .conflicts()
            .iter()
            .map(|conflict| conflict.name.clone())
            .collect();
        drop(registry);
        names.sort();
        names.dedup();
        names
    }

    /// Register a discovered resource in the registry.
    pub fn register_resource(&self, resource: McpResource) {
        self.resources.lock().unwrap().register(resource);
    }

    /// Register a discovered prompt, tagged with the server that offered it.
    pub fn register_prompt(&self, prompt: ServerPrompt) {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register(prompt);
    }

    /// Read one resource from a named server.
    ///
    /// Sends a live `resources/read` request, parses the first usable entry
    /// (`text` or base64 `blob`) of the response into [`McpResourceContent`],
    /// and caches it under the server that answered. `server_name` must name a
    /// configured server; use [`McpManager::read_resource_any`] when the caller
    /// only has a URI.
    pub async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
    ) -> Result<McpResourceContent, String> {
        let mut server = self
            .get_server(server_name)
            .ok_or_else(|| format!("MCP server '{server_name}' is not attached"))?;

        let response = server.read_resource(uri).await.map_err(|error| {
            format!("failed to read resource '{uri}' from server '{server_name}': {error}")
        })?;

        let content = parse_resource_contents(&response, uri)?;
        self.cache_resource(server_name, uri, content.clone());
        Ok(content)
    }

    /// Read a resource from whichever running server can serve it.
    ///
    /// Tries every running server in turn and returns the first successful
    /// read. The response is cached under the server that answered. The error
    /// lists each server's failure so a caller can tell "nothing is attached"
    /// from "every server refused this URI".
    pub async fn read_resource_any(&self, uri: &str) -> Result<McpResourceContent, String> {
        let names = self.list_servers();
        if names.is_empty() {
            return Err(format!(
                "no MCP server is attached; cannot read resource '{uri}'"
            ));
        }

        // A previous read is authoritative until `resources/updated`
        // invalidates it; avoid a round trip when the cache already has it.
        for name in &names {
            if let Some(content) = self.get_cached_resource(name, uri) {
                return Ok(content);
            }
        }

        let mut failures = Vec::new();
        let mut attempted = false;
        for name in names {
            if !self.is_running(&name) {
                continue;
            }
            attempted = true;
            match self.read_resource(&name, uri).await {
                Ok(content) => return Ok(content),
                Err(error) => failures.push(format!("{name}: {error}")),
            }
        }

        if !attempted {
            return Err(format!(
                "no MCP server is running; cannot read resource '{uri}'"
            ));
        }

        Err(format!(
            "no MCP server could read resource '{uri}': {}",
            failures.join("; ")
        ))
    }

    /// Resolve one prompt on a named server.
    ///
    /// Sends a live `prompts/get` request with `arguments` as its JSON object
    /// and parses the response into [`McpPromptResult`]. `server_name` and
    /// `prompt_name` are the configured server name and the raw prompt name the
    /// server advertised; neither is sanitised.
    ///
    /// The error names the server and the prompt, so a caller that tried more
    /// than one server can report each refusal separately.
    pub async fn get_prompt(
        &self,
        server_name: &str,
        prompt_name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<McpPromptResult, String> {
        let mut server = self
            .get_server(server_name)
            .ok_or_else(|| format!("MCP server '{server_name}' is not attached"))?;

        let response = server
            .get_prompt(prompt_name, &arguments)
            .await
            .map_err(|error| {
                format!("failed to get prompt '{prompt_name}' from server '{server_name}': {error}")
            })?;

        parse_prompt_result(&response).map_err(|error| {
            format!(
                "server '{server_name}' returned an invalid prompts/get response for \
                 '{prompt_name}': {error}"
            )
        })
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
            let pair = (info.server.clone(), info.name.clone());
            drop(tools);
            pair
        };

        let (running, channel) = {
            let server_entry = self.servers.get(&server_name)?;
            let server = server_entry.value();
            let state = (server.status == ServerStatus::Running, server.channel());
            drop(server_entry);
            state
        };

        if !running {
            return None;
        }

        Some(McpToolProxy::with_channel(
            &server_name,
            &tool_name_owned,
            channel?,
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

    /// List all registered resources with their discovered metadata.
    ///
    /// Unlike [`McpManager::list_resources`], which returns bare URIs, this
    /// lets a caller render a resource's name and MIME type instead of
    /// inventing them from the URI.
    #[must_use]
    pub fn list_resource_info(&self) -> Vec<McpResource> {
        self.resources.lock().unwrap().list().to_vec()
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

    /// All discovered prompts, each tagged with the server that offered it.
    #[must_use]
    pub fn list_prompts(&self) -> Vec<ServerPrompt> {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list()
            .to_vec()
    }

    /// Prompts offered by one server.
    #[must_use]
    pub fn prompts_for_server(&self, server: &str) -> Vec<McpPromptInfo> {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .for_server(server)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Look up one discovered prompt by server and name.
    #[must_use]
    pub fn prompt_info(&self, server: &str, name: &str) -> Option<McpPromptInfo> {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(server, name)
            .map(|entry| entry.prompt.clone())
    }

    /// Number of registered prompts across every server.
    #[must_use]
    pub fn prompt_count(&self) -> usize {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
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

    /// Install the handler that answers requests the servers make.
    ///
    /// This covers every server the manager knows, including ones already
    /// running and ones created later by [`McpManager::load_config`]. Calling
    /// it again replaces the handler. When no handler is installed at all,
    /// server requests are refused with `-32601` rather than left waiting.
    pub fn set_inbound_handler(&self, handler: Arc<dyn crate::channel::InboundRequestHandler>) {
        self.inbound_handler.install(handler);
    }

    /// Remove the inbound handler; later server requests are refused.
    pub fn clear_inbound_handler(&self) {
        self.inbound_handler.clear();
    }

    /// Install the router that sees every server notification.
    ///
    /// This covers servers already created by [`McpManager::load_config`] and
    /// any created later: they all hold the same shared handle. Calling it
    /// again replaces the router. Without one, notifications are handled by the
    /// manager alone and dropped after that, as before.
    pub fn set_notification_router(&self, router: Arc<dyn ServerNotificationRouter>) {
        *self
            .notification_router
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(router);
    }

    /// Remove the notification router; later notifications reach only the
    /// manager.
    pub fn clear_notification_router(&self) {
        *self
            .notification_router
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Whether a handler is installed for server requests.
    #[must_use]
    pub fn has_inbound_handler(&self) -> bool {
        self.inbound_handler.has_handler()
    }

    /// Access the elicitation manager.
    #[must_use]
    pub const fn elicitation(&self) -> &ElicitationManager {
        &self.elicitation
    }

    /// Mutable access to the elicitation manager.
    #[must_use]
    pub const fn elicitation_mut(&mut self) -> &mut ElicitationManager {
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

/// Parse the `contents` array of a `resources/read` response.
///
/// MCP allows a single read to return several entries; the first with actual
/// content wins. A `text` entry and a base64 `blob` entry are both supported.
/// Entries that carry neither are skipped, and both an empty list and a list
/// of only unusable entries are reported rather than returned as empty
/// content.
fn parse_resource_contents(
    response: &serde_json::Value,
    requested_uri: &str,
) -> Result<McpResourceContent, String> {
    let contents = response
        .get("contents")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            format!("resources/read response for '{requested_uri}' has no 'contents' array")
        })?;

    if contents.is_empty() {
        return Err(format!(
            "the server returned no contents for resource '{requested_uri}'"
        ));
    }

    for entry in contents {
        let text = entry
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let blob = entry
            .get("blob")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        if text.is_none() && blob.is_none() {
            continue;
        }

        let uri = entry
            .get("uri")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(requested_uri)
            .to_string();
        let mime_type = entry
            .get("mimeType")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_string();

        return Ok(McpResourceContent {
            uri,
            mime_type,
            text,
            blob,
        });
    }

    Err(format!(
        "the server returned no readable contents for resource '{requested_uri}' \
         (entries had neither 'text' nor 'blob')"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpServerConfig, McpTransport};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static MOCK_PROMPT_ID: AtomicU64 = AtomicU64::new(0);

    /// A stdio MCP server that advertises two prompts and resolves one.
    ///
    /// `MOCK_PROMPTS_BROKEN_LIST=1` makes `prompts/list` fail, which is how a
    /// per-server discovery failure is exercised without a second script.
    const MOCK_PROMPTS_SERVER: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

BROKEN_LIST = os.environ.get("MOCK_PROMPTS_BROKEN_LIST") == "1"

def send_response(req_id, result):
    print(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": result}), flush=True)

def send_error(req_id, code, message):
    print(json.dumps({"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}}), flush=True)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except json.JSONDecodeError:
        continue

    method = req.get("method", "")
    req_id = req.get("id")

    if method == "initialize":
        send_response(req_id, {
            "protocolVersion": "2024-11-05",
            "capabilities": {"prompts": {}},
            "serverInfo": {"name": "mock-prompts", "version": "1.0"}
        })
    elif method == "initialized":
        pass
    elif method == "tools/list":
        send_response(req_id, {"tools": []})
    elif method == "resources/list":
        send_response(req_id, {"resources": []})
    elif method == "prompts/list":
        if BROKEN_LIST:
            send_error(req_id, -32000, "prompt listing is unavailable")
        else:
            send_response(req_id, {"prompts": [
                {
                    "name": "review",
                    "description": "Review a file",
                    "arguments": [
                        {"name": "path", "description": "File to review", "required": True},
                        {"name": "style"}
                    ]
                },
                {"name": "broken"}
            ]})
    elif method == "prompts/get":
        params = req.get("params", {})
        name = params.get("name", "")
        args = params.get("arguments", {})
        if name == "review":
            path = args.get("path", "")
            style = args.get("style", "default")
            send_response(req_id, {
                "description": "Review " + path,
                "messages": [
                    {"role": "user", "content": {"type": "text", "text": "Please review " + path}},
                    {"role": "assistant", "content": {"type": "text", "text": "Style: " + style}}
                ]
            })
        elif name == "broken":
            send_error(req_id, -32000, "prompt backend failed")
        else:
            send_error(req_id, -32602, "unknown prompt: " + name)
    else:
        send_error(req_id, -32601, "Method " + method + " not found")
"#;

    fn mock_prompts_server_path() -> PathBuf {
        let id = MOCK_PROMPT_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("coderex_mock_prompts_{id}.py"));
        std::fs::write(&path, MOCK_PROMPTS_SERVER).expect("write mock prompt server");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path)
                .expect("stat mock prompt server")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("chmod mock prompt server");
        }
        path
    }

    fn prompts_config(servers: &[(&str, &PathBuf, HashMap<String, String>)]) -> McpConfig {
        let mut config = McpConfig::new();
        for (name, script, env) in servers {
            config.add_server(
                *name,
                McpServerConfig {
                    command: script.to_str().expect("script path is utf-8").into(),
                    args: Vec::new(),
                    env: env.clone(),
                    ..Default::default()
                },
            );
        }
        config
    }

    #[test]
    fn new_manager_is_empty() {
        let mgr = McpManager::new();
        assert_eq!(mgr.list_servers().len(), 0);
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
    fn tool_conflicts_are_sorted_and_deduplicated() {
        use crate::proxy::{clear_mcp_prefix_override, set_mcp_prefix_enabled};

        let mgr = McpManager::new();
        assert_eq!(mgr.tool_conflicts().len(), 0);

        // Without the server prefix two servers cannot both register a raw
        // name; the later registration is recorded as a conflict.
        set_mcp_prefix_enabled(false);
        let tool = |server: &str, name: &str| McpToolInfo {
            server: server.into(),
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
        };
        mgr.register_tool(tool("one", "zeta"));
        mgr.register_tool(tool("two", "zeta"));
        mgr.register_tool(tool("three", "alpha"));
        mgr.register_tool(tool("four", "alpha"));
        // A third server offering an already-conflicting name must not make
        // the same name appear twice in the surfaced list.
        mgr.register_tool(tool("five", "zeta"));

        assert_eq!(
            mgr.tool_conflicts(),
            vec!["alpha".to_string(), "zeta".to_string()],
            "conflict names must be deduplicated and in deterministic order"
        );

        clear_mcp_prefix_override();
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

    /// `notifications/resources/updated` removes that resource from the cache
    /// so the next read goes back to the server.
    #[tokio::test]
    async fn resources_updated_invalidates_the_cached_content() {
        let mgr = McpManager::new();
        let content = |uri: &str| crate::resources::McpResourceContent {
            uri: uri.to_string(),
            mime_type: "text/plain".to_string(),
            text: Some("stale".to_string()),
            blob: None,
        };
        mgr.cache_resource("remote", "file:///a", content("file:///a"));
        mgr.cache_resource("remote", "file:///b", content("file:///b"));

        let handler = PerServerNotificationHandler {
            server: "remote".to_string(),
            sink: mgr.notification_sink(),
        };
        crate::channel::ServerNotificationHandler::handle_notification(
            &handler,
            "notifications/resources/updated",
            Some(&serde_json::json!({"uri": "file:///a"})),
        )
        .await;

        assert!(
            mgr.get_cached_resource("remote", "file:///a").is_none(),
            "the updated resource must be invalidated"
        );
        assert!(mgr.get_cached_resource("remote", "file:///b").is_some());
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

    /// Records every notification the router is offered.
    #[derive(Debug, Default)]
    struct RecordingRouter {
        seen: std::sync::Mutex<Vec<(String, String, Option<serde_json::Value>)>>,
    }

    impl ServerNotificationRouter for RecordingRouter {
        fn route(&self, server: &str, method: &str, params: Option<&serde_json::Value>) {
            self.seen.lock().unwrap().push((
                server.to_string(),
                method.to_string(),
                params.cloned(),
            ));
        }
    }

    /// A notification reaches an installed router — the seam the CLI uses to
    /// hand MCP notifications to the plugin channel layer. Both a method the
    /// manager acts on itself and one it does not understand are routed.
    #[tokio::test]
    async fn notifications_reach_an_installed_router() {
        let mgr = McpManager::new();
        let router = Arc::new(RecordingRouter::default());
        mgr.set_notification_router(router.clone());

        let handler = PerServerNotificationHandler {
            server: "remote".to_string(),
            sink: mgr.notification_sink(),
        };

        crate::channel::ServerNotificationHandler::handle_notification(
            &handler,
            "notifications/resources/updated",
            Some(&serde_json::json!({"uri": "file:///a"})),
        )
        .await;
        crate::channel::ServerNotificationHandler::handle_notification(
            &handler,
            "notifications/message",
            Some(&serde_json::json!({"level": "info"})),
        )
        .await;

        let seen = router.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0].0, "remote");
        assert_eq!(seen[0].1, "notifications/resources/updated");
        assert_eq!(seen[0].2.as_ref().unwrap()["uri"], "file:///a");
        assert_eq!(seen[1].1, "notifications/message");
        assert_eq!(seen[1].2.as_ref().unwrap()["level"], "info");

        // Removing the router stops delivery rather than leaving a stale one
        // installed.
        mgr.clear_notification_router();
        crate::channel::ServerNotificationHandler::handle_notification(
            &handler,
            "notifications/message",
            None,
        )
        .await;
        assert_eq!(router.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn prompts_are_discovered_and_resolved_through_the_manager() {
        let script = mock_prompts_server_path();
        let mgr = McpManager::new();
        mgr.load_config(&prompts_config(&[("mock", &script, HashMap::new())]));
        mgr.start_all().await;

        assert_eq!(mgr.prompt_count(), 2, "prompts/list should be discovered");
        let prompts = mgr.prompts_for_server("mock");
        let review = prompts
            .iter()
            .find(|prompt| prompt.name == "review")
            .expect("the mock advertises 'review'");
        assert_eq!(review.description.as_deref(), Some("Review a file"));
        assert_eq!(review.arguments.len(), 2);
        assert!(review.arguments[0].required);

        let mut arguments = serde_json::Map::new();
        arguments.insert("path".into(), serde_json::json!("src/lib.rs"));
        arguments.insert("style".into(), serde_json::json!("terse"));
        let result = mgr
            .get_prompt("mock", "review", arguments)
            .await
            .expect("prompts/get should answer");
        assert_eq!(result.description.as_deref(), Some("Review src/lib.rs"));
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.messages[0].role, "user");
        let rendered = result.render_text();
        assert!(rendered.contains("Please review src/lib.rs"), "{rendered}");
        assert!(rendered.contains("Style: terse"), "{rendered}");

        mgr.stop_all().await;
    }

    #[tokio::test]
    async fn a_failing_server_is_reported_without_hiding_the_others_prompts() {
        let good = mock_prompts_server_path();
        let bad = mock_prompts_server_path();
        let mut broken = HashMap::new();
        broken.insert("MOCK_PROMPTS_BROKEN_LIST".to_string(), "1".to_string());

        let mgr = McpManager::new();
        mgr.load_config(&prompts_config(&[
            ("good", &good, HashMap::new()),
            ("bad", &bad, broken),
        ]));
        mgr.start_all().await;

        assert_eq!(
            mgr.prompts_for_server("good").len(),
            2,
            "the healthy server's prompts must still be discovered"
        );
        assert!(
            mgr.prompts_for_server("bad").is_empty(),
            "a server that fails prompts/list contributes nothing"
        );
        assert!(mgr.prompt_info("good", "review").is_some());
        assert!(mgr.prompt_info("bad", "review").is_none());

        // A live refusal is reported against the server that gave it, not as a
        // global "MCP is broken" failure.
        let error = mgr
            .get_prompt("good", "broken", serde_json::Map::new())
            .await
            .unwrap_err();
        assert!(error.contains("'good'"), "{error}");
        assert!(error.contains("prompt backend failed"), "{error}");

        let error = mgr
            .get_prompt("absent", "review", serde_json::Map::new())
            .await
            .unwrap_err();
        assert!(error.contains("not attached"), "{error}");

        mgr.stop_all().await;
    }
}
