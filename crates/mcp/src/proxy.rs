//! MCP tool proxy — tool discovery, registration, and call forwarding.

use crate::channel::JsonRpcChannel;
use crate::server::McpServer;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Environment variable that turns the `mcp__<server>__<tool>` prefix off.
///
/// Set it to anything other than `0`/`false` (an empty value counts as "set")
/// to address MCP tools by their raw server-side name. Mirrors the reference
/// SDK's `CLAUDE_AGENT_SDK_MCP_NO_PREFIX`.
pub const MCP_NO_PREFIX_ENV: &str = "CLAUDE_AGENT_SDK_MCP_NO_PREFIX";

thread_local! {
    /// Per-thread override of the env var, so a test can exercise no-prefix
    /// mode without racing every other test in the process through a global.
    static PREFIX_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether the `mcp__<server>__<tool>` prefix is in force.
///
/// Prefixing is the default because it is load-bearing: two servers may expose
/// the same raw tool name, and the prefix is what keeps them apart. Turning it
/// off is opt-in, and the cost of a collision is paid in
/// [`McpToolRegistry::register`], not silently.
#[must_use]
pub fn mcp_prefix_enabled() -> bool {
    if let Some(forced) = PREFIX_OVERRIDE.with(std::cell::Cell::get) {
        return forced;
    }
    // Unset means prefixed: the reference's spelling of "no prefix" is the
    // variable being present, and anything but an explicit off counts as
    // present.
    std::env::var(MCP_NO_PREFIX_ENV).map_or(true, |value| !no_prefix_value_is_set(&value))
}

/// Whether a value of [`MCP_NO_PREFIX_ENV`] asks for no prefix.
fn no_prefix_value_is_set(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no"
    )
}

/// Force the prefix on or off for this thread. Used by tests; production reads
/// the environment.
pub fn set_mcp_prefix_enabled(enabled: bool) {
    PREFIX_OVERRIDE.with(|cell| cell.set(Some(enabled)));
}

/// Drop any thread-local override, returning to the environment.
pub fn clear_mcp_prefix_override() {
    PREFIX_OVERRIDE.with(|cell| cell.set(None));
}

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

impl McpToolInfo {
    /// Public name used by the model and permission rules.
    ///
    /// Normally the collision-free `mcp__<server>__<tool>`; the raw name when
    /// the prefix is switched off.
    ///
    /// The raw `name` field remains unchanged either way, because it is what
    /// the MCP server expects in `tools/call`.
    #[must_use]
    pub fn qualified_name(&self) -> String {
        if mcp_prefix_enabled() {
            self.prefixed_name()
        } else {
            self.name.clone()
        }
    }

    /// The server-qualified name, whatever the prefix switch says.
    ///
    /// This is the unambiguous spelling, and the one to use when a raw name
    /// turns out not to be unique.
    #[must_use]
    pub fn prefixed_name(&self) -> String {
        format!("mcp__{}__{}", self.server, self.name)
    }
}

/// A tool whose raw name is not unique, in no-prefix mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolNameConflict {
    /// The raw name two servers both expose.
    pub name: String,
    /// The server that already holds the name.
    pub held_by: String,
    /// The server whose tool was refused registration.
    pub refused: String,
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
        drop(ch);
        Ok(result)
    }
}

/// Tool registry for MCP-discovered tools.
#[derive(Debug, Default)]
pub struct McpToolRegistry {
    tools: HashMap<String, McpToolInfo>,
    conflicts: Vec<McpToolNameConflict>,
}

impl McpToolRegistry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            conflicts: Vec::new(),
        }
    }

    /// Register a discovered tool under its public name.
    ///
    /// Registering the same server's tool again replaces it, which is what a
    /// reconnect needs. In no-prefix mode two *different* servers can offer the
    /// same raw name, and then the name can only address one of them: rather
    /// than let the second registration quietly take over the name — and route
    /// calls to the wrong server — the newcomer is refused and recorded in
    /// [`Self::conflicts`]. Turn the prefix back on to reach it.
    pub fn register(&mut self, info: McpToolInfo) {
        let public = info.qualified_name();
        if let Some(existing) = self.tools.get(&public)
            && existing.server != info.server
        {
            tracing::warn!(
                tool = %public,
                held_by = %existing.server,
                refused = %info.server,
                "MCP tool name is not unique without the server prefix; \
                 the later registration was refused"
            );
            self.conflicts.push(McpToolNameConflict {
                name: info.name,
                held_by: existing.server.clone(),
                refused: info.server,
            });
            return;
        }
        self.tools.insert(public, info);
    }

    /// Names that could not be registered because no-prefix mode left them
    /// ambiguous. Empty in the default, prefixed mode.
    #[must_use]
    pub fn conflicts(&self) -> &[McpToolNameConflict] {
        &self.conflicts
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

    /// Remove every tool a server exposed.
    ///
    /// A `tools/list_changed` notification means the previous list is no
    /// longer authoritative: tools the server removed must disappear too, not
    /// just be shadowed by the ones it still lists.
    pub fn unregister_server(&mut self, server: &str) -> usize {
        let before = self.tools.len();
        self.tools.retain(|_, info| info.server != server);
        before - self.tools.len()
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
        let got = reg.get("mcp__filesystem__read_file").unwrap();
        assert_eq!(got.server, "filesystem");
        assert_eq!(got.description, "Read a file");
    }

    #[test]
    fn same_raw_tool_name_is_namespaced_by_server() {
        let mut reg = McpToolRegistry::new();
        reg.register(sample_tool());
        reg.register(McpToolInfo {
            server: "other".into(),
            ..sample_tool()
        });

        assert_eq!(reg.len(), 2);
        assert!(reg.get("read_file").is_none());
        assert_eq!(
            reg.get("mcp__filesystem__read_file")
                .map(|info| info.server.as_str()),
            Some("filesystem")
        );
        assert_eq!(
            reg.get("mcp__other__read_file")
                .map(|info| info.server.as_str()),
            Some("other")
        );
    }

    #[test]
    fn unregister() {
        let mut reg = McpToolRegistry::new();
        reg.register(sample_tool());
        let removed = reg.unregister("mcp__filesystem__read_file");
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

    /// The switch exists and defaults to the prefixed name.
    #[test]
    fn the_prefix_is_on_unless_the_environment_says_otherwise() {
        assert!(mcp_prefix_enabled());
        assert_eq!(sample_tool().qualified_name(), "mcp__filesystem__read_file");
        assert_eq!(sample_tool().prefixed_name(), "mcp__filesystem__read_file");

        set_mcp_prefix_enabled(false);
        assert!(!mcp_prefix_enabled());
        assert_eq!(sample_tool().qualified_name(), "read_file");
        // The unambiguous spelling is still available.
        assert_eq!(sample_tool().prefixed_name(), "mcp__filesystem__read_file");

        clear_mcp_prefix_override();
        assert!(mcp_prefix_enabled());
    }

    #[test]
    fn the_no_prefix_env_value_only_yields_to_an_explicit_off() {
        assert!(no_prefix_value_is_set("1"));
        assert!(no_prefix_value_is_set("true"));
        assert!(no_prefix_value_is_set(""));
        assert!(no_prefix_value_is_set("yes"));
        assert!(!no_prefix_value_is_set("0"));
        assert!(!no_prefix_value_is_set("false"));
        assert!(!no_prefix_value_is_set("FALSE"));
    }

    /// Without the prefix, two servers offering the same raw name cannot both
    /// be addressed by it. The later one is refused rather than silently
    /// taking the name over, which would send its calls to the other server.
    #[test]
    fn a_raw_name_two_servers_share_is_refused_rather_than_routed_wrongly() {
        set_mcp_prefix_enabled(false);
        let mut reg = McpToolRegistry::new();
        reg.register(sample_tool());
        reg.register(McpToolInfo {
            server: "other".into(),
            ..sample_tool()
        });

        assert_eq!(reg.len(), 1, "the ambiguous tool was registered anyway");
        assert_eq!(
            reg.get("read_file").map(|info| info.server.as_str()),
            Some("filesystem"),
            "the name must keep addressing the server that already held it"
        );
        assert_eq!(
            reg.conflicts(),
            [McpToolNameConflict {
                name: "read_file".into(),
                held_by: "filesystem".into(),
                refused: "other".into(),
            }]
        );

        clear_mcp_prefix_override();
    }

    /// In the default mode the same two tools coexist, and nothing is refused.
    #[test]
    fn the_prefix_keeps_same_named_tools_apart_without_conflict() {
        let mut reg = McpToolRegistry::new();
        reg.register(sample_tool());
        reg.register(McpToolInfo {
            server: "other".into(),
            ..sample_tool()
        });

        assert_eq!(reg.len(), 2);
        assert!(reg.conflicts().is_empty());
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
