//! MCP tool proxy — tool discovery, registration, and call forwarding.

use crate::channel::JsonRpcChannel;
use crate::server::McpServer;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// The prefix MCP tools are exposed under. Empty means "a tool is addressed
/// by its raw server-side name".
///
/// The default is `mcp__`, which makes the public name
/// `mcp__<server>__<tool>`: two servers may expose the same raw tool name, and
/// the server name is what keeps them apart.
pub const MCP_PREFIX_ENV: &str = "CODEREX_MCP_PREFIX";
/// The prefix used when [`MCP_PREFIX_ENV`] is unset.
pub const DEFAULT_MCP_PREFIX: &str = "mcp__";

/// Environment variable that turns the prefix off, spelled the way the
/// reference SDK spells it.
///
/// Set it to anything other than `0`/`false` (an empty value counts as "set")
/// to address MCP tools by their raw server-side name. It is consulted only
/// when [`MCP_PREFIX_ENV`] is unset, so a configured prefix always wins.
pub const MCP_NO_PREFIX_ENV: &str = "CLAUDE_AGENT_SDK_MCP_NO_PREFIX";

/// A per-thread override of the env vars, so a test can exercise a custom or
/// disabled prefix without racing every other test through the global
/// environment.
#[derive(Clone)]
enum PrefixOverride {
    /// Read the environment.
    Environment,
    /// No prefix: raw tool names.
    Disabled,
    /// This prefix.
    Custom(String),
}

thread_local! {
    static PREFIX_OVERRIDE: std::cell::RefCell<PrefixOverride> =
        const { std::cell::RefCell::new(PrefixOverride::Environment) };
}

/// The prefix in force for this thread.
///
/// Precedence: a thread-local test override, then [`MCP_PREFIX_ENV`], then
/// [`MCP_NO_PREFIX_ENV`], then [`DEFAULT_MCP_PREFIX`]. An empty
/// [`MCP_PREFIX_ENV`] is a valid way to ask for raw names; a value with
/// anything other than ASCII letters, digits and `_` is ignored with a warning
/// (it would produce a name no tool registry can address).
#[must_use]
pub fn mcp_prefix() -> String {
    match PREFIX_OVERRIDE.with(|cell| cell.borrow().clone()) {
        PrefixOverride::Custom(prefix) => return prefix,
        PrefixOverride::Disabled => return String::new(),
        PrefixOverride::Environment => {}
    }
    let configured = std::env::var(MCP_PREFIX_ENV).ok();
    let legacy = std::env::var(MCP_NO_PREFIX_ENV).ok();
    resolve_prefix(configured.as_deref(), legacy.as_deref())
}

/// The precedence rule, pure so it can be tested without touching the
/// process environment.
#[must_use]
pub fn resolve_prefix(configured: Option<&str>, legacy_no_prefix: Option<&str>) -> String {
    if let Some(value) = configured {
        if value.is_empty() {
            return String::new();
        }
        if prefix_is_usable(value) {
            return value.to_string();
        }
        tracing::warn!(
            value = %value,
            "ignoring {MCP_PREFIX_ENV}: a prefix may only contain ASCII letters, digits and _"
        );
        return DEFAULT_MCP_PREFIX.to_string();
    }
    if let Some(value) = legacy_no_prefix
        && no_prefix_value_is_set(value)
    {
        return String::new();
    }
    DEFAULT_MCP_PREFIX.to_string()
}

/// Whether a configured prefix can produce a name tools can be registered
/// under.
fn prefix_is_usable(value: &str) -> bool {
    value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Whether the prefix is in force at all.
#[must_use]
pub fn mcp_prefix_enabled() -> bool {
    !mcp_prefix().is_empty()
}

/// Whether a value of [`MCP_NO_PREFIX_ENV`] asks for no prefix.
fn no_prefix_value_is_set(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no"
    )
}

/// The public name of a tool: `<prefix><server>__<tool>`, or the raw name when
/// the prefix is empty.
///
/// This is what the model, permission rules, hook actions and prompt slash
/// commands all use; the raw `name` is still what `tools/call` sends.
#[must_use]
pub fn qualified_tool_name(server: &str, name: &str) -> String {
    let prefix = mcp_prefix();
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}{server}__{name}")
    }
}

/// Force a prefix for this thread. `None` restores the environment;
/// `Some("")` asks for raw names. Used by tests; production reads the
/// environment.
pub fn set_mcp_prefix_for_test(prefix: Option<&str>) {
    let override_value = match prefix {
        Some("") => PrefixOverride::Disabled,
        Some(prefix) => PrefixOverride::Custom(prefix.to_string()),
        None => PrefixOverride::Environment,
    };
    PREFIX_OVERRIDE.with(|cell| cell.replace(override_value));
}

/// Force the default prefix on or off for this thread. Used by tests.
pub fn set_mcp_prefix_enabled(enabled: bool) {
    set_mcp_prefix_for_test(if enabled {
        Some(DEFAULT_MCP_PREFIX)
    } else {
        Some("")
    });
}

/// Drop any thread-local override, returning to the environment.
pub fn clear_mcp_prefix_override() {
    PREFIX_OVERRIDE.with(|cell| cell.replace(PrefixOverride::Environment));
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
        qualified_tool_name(&self.server, &self.name)
    }

    /// The server-qualified name, whatever the prefix switch says.
    ///
    /// This is the unambiguous spelling, and the one to use when a raw name
    /// turns out not to be unique.
    #[must_use]
    pub fn prefixed_name(&self) -> String {
        format!("{DEFAULT_MCP_PREFIX}{}__{}", self.server, self.name)
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

    /// The prefix is configurable: unset means the default, a value replaces
    /// it, and an empty value means raw names.
    #[test]
    fn a_configured_prefix_replaces_the_default() {
        assert_eq!(resolve_prefix(None, None), DEFAULT_MCP_PREFIX);
        assert_eq!(resolve_prefix(Some("coderex_"), None), "coderex_");
        assert_eq!(resolve_prefix(Some(""), None), "");
        set_mcp_prefix_for_test(Some("coderex_"));
        assert_eq!(
            qualified_tool_name("filesystem", "read_file"),
            "coderex_filesystem__read_file"
        );
        assert_eq!(
            sample_tool().qualified_name(),
            "coderex_filesystem__read_file"
        );
        // The unambiguous spelling is still the default one.
        assert_eq!(sample_tool().prefixed_name(), "mcp__filesystem__read_file");
        clear_mcp_prefix_override();
        assert_eq!(sample_tool().qualified_name(), "mcp__filesystem__read_file");
    }

    /// `CODEREX_MCP_PREFIX` wins over the reference's no-prefix switch, and an
    /// empty prefix is the way to ask for raw names.
    #[test]
    fn the_configured_prefix_wins_over_the_legacy_switch() {
        assert_eq!(resolve_prefix(Some("x_"), Some("1")), "x_");
        assert_eq!(resolve_prefix(Some(""), Some("0")), "");
        assert_eq!(resolve_prefix(None, Some("1")), "");
        assert_eq!(resolve_prefix(None, Some("0")), DEFAULT_MCP_PREFIX);
    }

    /// A prefix that cannot appear in a tool name falls back to the default
    /// rather than producing names nothing can address.
    #[test]
    fn an_unusable_prefix_falls_back_to_the_default() {
        assert_eq!(resolve_prefix(Some("bad prefix"), None), DEFAULT_MCP_PREFIX);
        assert_eq!(resolve_prefix(Some("bad-prefix"), None), DEFAULT_MCP_PREFIX);
    }

    /// A custom prefix still keeps two servers' same-named tools apart, and
    /// without one the collision is refused exactly as before.
    #[test]
    fn a_custom_prefix_keeps_same_named_tools_apart() {
        set_mcp_prefix_for_test(Some("custom_"));
        let mut reg = McpToolRegistry::new();
        reg.register(sample_tool());
        reg.register(McpToolInfo {
            server: "other".into(),
            ..sample_tool()
        });
        assert_eq!(reg.len(), 2);
        assert!(reg.get("custom_filesystem__read_file").is_some());
        assert!(reg.conflicts().is_empty());
        clear_mcp_prefix_override();
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
