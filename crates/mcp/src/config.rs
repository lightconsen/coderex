//! MCP configuration — `.mcp.json` parsing.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Transport used to reach an MCP server.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// Launch a subprocess and exchange newline-delimited JSON-RPC over stdio.
    #[default]
    Stdio,
    /// Connect to a classic (legacy) HTTP+SSE endpoint: a long-lived GET
    /// event stream announces a separate POST message endpoint, and responses
    /// are correlated by JSON-RPC id.
    Sse,
    /// Connect to a remote streamable-HTTP endpoint.
    #[serde(alias = "streamable-http")]
    Http,
}

/// Configuration for a single MCP server.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Transport to use. Defaults to stdio for backwards compatibility.
    #[serde(default, rename = "type")]
    pub transport: McpTransport,
    /// Remote HTTP/SSE endpoint. Required when `transport` is not stdio.
    #[serde(default)]
    pub url: Option<String>,
    /// Additional HTTP headers sent on every request.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// The command to run the server.
    #[serde(default)]
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for the server process.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// The full MCP configuration file structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    /// Named MCP server configurations.
    #[serde(default, alias = "mcpServers")]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl McpConfig {
    /// Create a new empty MCP config.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mcp_servers: HashMap::new(),
        }
    }

    /// Load from a file path.
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        let config: McpConfig = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(config)
    }

    /// Save to a file path.
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, content)
    }

    /// Get the default config file path in the current directory.
    #[must_use]
    pub fn default_path(cwd: &Path) -> PathBuf {
        cwd.join(".mcp.json")
    }

    /// Add a server configuration.
    pub fn add_server(&mut self, name: impl Into<String>, config: McpServerConfig) {
        self.mcp_servers.insert(name.into(), config);
    }

    /// Add a remote (HTTP or SSE) server.
    ///
    /// `transport` must be [`McpTransport::Http`] or [`McpTransport::Sse`];
    /// `url` must be a non-empty `http` or `https` URL. `headers` are sent on
    /// every request the server receives and are stored verbatim, so a caller
    /// can pass an `Authorization` value straight through.
    ///
    /// This is the constructor behind `coderex mcp add --url … --transport …`;
    /// unlike [`Self::add_server`] it validates the input and reports a message
    /// suitable for the CLI.
    pub fn add_remote_server(
        &mut self,
        name: impl Into<String>,
        url: impl Into<String>,
        transport: McpTransport,
        headers: HashMap<String, String>,
    ) -> Result<(), String> {
        if transport == McpTransport::Stdio {
            return Err(
                "a remote MCP server requires the 'http' or 'sse' transport, not 'stdio'"
                    .to_string(),
            );
        }

        let url = url.into();
        let url = url.trim();
        if url.is_empty() {
            return Err("a remote MCP server requires a non-empty URL".to_string());
        }
        let parsed =
            url::Url::parse(url).map_err(|error| format!("invalid MCP server URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!(
                "unsupported MCP server URL scheme '{}'; expected http or https",
                parsed.scheme()
            ));
        }

        self.mcp_servers.insert(
            name.into(),
            McpServerConfig {
                transport,
                url: Some(url.to_string()),
                headers,
                command: String::new(),
                args: Vec::new(),
                env: HashMap::new(),
            },
        );
        Ok(())
    }

    /// Add a stdio server from a command line.
    ///
    /// The command is split on whitespace: the first token is the executable
    /// and the rest become arguments. An empty (or all-whitespace) command is
    /// rejected. This is the constructor behind `coderex mcp add` without
    /// `--url`.
    pub fn add_stdio_server(
        &mut self,
        name: impl Into<String>,
        command: &str,
    ) -> Result<(), String> {
        let mut parts = command.split_whitespace();
        let executable = parts
            .next()
            .ok_or_else(|| "a stdio MCP server requires a non-empty command".to_string())?;

        self.mcp_servers.insert(
            name.into(),
            McpServerConfig {
                transport: McpTransport::Stdio,
                url: None,
                headers: HashMap::new(),
                command: executable.to_string(),
                args: parts.map(String::from).collect(),
                env: HashMap::new(),
            },
        );
        Ok(())
    }

    /// Remove a server configuration.
    pub fn remove_server(&mut self, name: &str) -> Option<McpServerConfig> {
        self.mcp_servers.remove(name)
    }

    /// Get a server configuration by name.
    #[must_use]
    pub fn get_server(&self, name: &str) -> Option<&McpServerConfig> {
        self.mcp_servers.get(name)
    }

    /// List all server names.
    #[must_use]
    pub fn server_names(&self) -> Vec<&str> {
        self.mcp_servers.keys().map(String::as_str).collect()
    }

    /// Check if the config has any servers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mcp_servers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> McpConfig {
        let mut config = McpConfig::new();
        config.add_server(
            "filesystem",
            McpServerConfig {
                command: "npx".into(),
                args: vec![
                    "@modelcontextprotocol/server-filesystem".into(),
                    "/tmp".into(),
                ],
                env: HashMap::new(),
                ..Default::default()
            },
        );
        config
    }

    #[test]
    fn new_config_is_empty() {
        let config = McpConfig::new();
        assert!(config.is_empty());
    }

    #[test]
    fn add_and_get_server() {
        let config = sample_config();
        assert!(!config.is_empty());
        let server = config.get_server("filesystem").unwrap();
        assert_eq!(server.command, "npx");
        assert!(config.get_server("nonexistent").is_none());
    }

    #[test]
    fn remove_server() {
        let mut config = sample_config();
        let removed = config.remove_server("filesystem");
        assert!(removed.is_some());
        assert!(config.is_empty());
    }

    #[test]
    fn server_names() {
        let mut config = McpConfig::new();
        config.add_server(
            "a",
            McpServerConfig {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
                ..Default::default()
            },
        );
        config.add_server(
            "b",
            McpServerConfig {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
                ..Default::default()
            },
        );
        let names = config.server_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        let config = sample_config();
        config.save(&path).unwrap();
        let loaded = McpConfig::load(&path).unwrap();
        assert!(!loaded.is_empty());
        assert_eq!(loaded.get_server("filesystem").unwrap().command, "npx");
    }

    #[test]
    fn parses_http_transport_url_headers_and_alias() {
        let config: McpConfig = serde_json::from_str(
            r#"{
                "mcpServers": {
                    "remote": {
                        "type": "streamable-http",
                        "url": "http://localhost:3000/mcp",
                        "headers": { "Authorization": "Bearer test" }
                    }
                }
            }"#,
        )
        .unwrap();
        let server = config.get_server("remote").unwrap();
        assert_eq!(server.transport, McpTransport::Http);
        assert_eq!(server.url.as_deref(), Some("http://localhost:3000/mcp"));
        assert_eq!(server.headers["Authorization"], "Bearer test");
        assert!(server.command.is_empty());
    }

    /// `http` and its `streamable-http` alias are the streamable transport;
    /// `sse` is the classic HTTP+SSE transport, not an alias for it.
    #[test]
    fn transport_names_select_distinct_transports() {
        let config: McpConfig = serde_json::from_str(
            r#"{
                "mcpServers": {
                    "streamable": { "type": "http", "url": "http://localhost/streamable" },
                    "aliased": { "type": "streamable-http", "url": "http://localhost/aliased" },
                    "legacy": { "type": "sse", "url": "http://localhost/sse" },
                    "defaulted": { "url": "http://localhost/default" }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            config.get_server("streamable").unwrap().transport,
            McpTransport::Http
        );
        assert_eq!(
            config.get_server("aliased").unwrap().transport,
            McpTransport::Http
        );
        assert_eq!(
            config.get_server("legacy").unwrap().transport,
            McpTransport::Sse,
            "`type: \"sse\"` must not be treated as streamable HTTP"
        );
        assert_eq!(
            config.get_server("defaulted").unwrap().transport,
            McpTransport::Stdio
        );
    }

    #[test]
    fn add_remote_server_preserves_url_transport_and_headers() {
        let mut config = McpConfig::new();
        let headers = HashMap::from([
            ("Authorization".to_string(), "Bearer test".to_string()),
            ("X-Tenant".to_string(), "acme".to_string()),
        ]);

        config
            .add_remote_server(
                "remote",
                "https://example.com/mcp",
                McpTransport::Http,
                headers.clone(),
            )
            .unwrap();

        let server = config.get_server("remote").unwrap();
        assert_eq!(server.transport, McpTransport::Http);
        assert_eq!(server.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(server.headers, headers, "headers must be stored verbatim");
        assert!(server.command.is_empty());
        assert!(server.args.is_empty());
    }

    #[test]
    fn add_remote_server_accepts_sse() {
        let mut config = McpConfig::new();
        config
            .add_remote_server(
                "legacy",
                "http://localhost:8080/sse",
                McpTransport::Sse,
                HashMap::new(),
            )
            .unwrap();
        assert_eq!(
            config.get_server("legacy").unwrap().transport,
            McpTransport::Sse
        );
    }

    #[test]
    fn add_remote_server_rejects_url_with_stdio_transport() {
        let mut config = McpConfig::new();
        let error = config
            .add_remote_server(
                "bad",
                "http://localhost/mcp",
                McpTransport::Stdio,
                HashMap::new(),
            )
            .unwrap_err();
        assert!(error.contains("stdio"), "unexpected error: {error}");
        assert!(
            config.get_server("bad").is_none(),
            "a rejected server must not be added"
        );
    }

    #[test]
    fn add_remote_server_rejects_a_missing_or_empty_url() {
        for url in ["", "   "] {
            for transport in [McpTransport::Http, McpTransport::Sse] {
                let mut config = McpConfig::new();
                let error = config
                    .add_remote_server("bad", url, transport, HashMap::new())
                    .unwrap_err();
                assert!(error.contains("non-empty URL"), "unexpected error: {error}");
                assert!(config.get_server("bad").is_none());
            }
        }
    }

    #[test]
    fn add_remote_server_rejects_non_http_schemes() {
        for url in ["ftp://example.com/mcp", "file:///tmp/socket"] {
            let mut config = McpConfig::new();
            let error = config
                .add_remote_server("bad", url, McpTransport::Http, HashMap::new())
                .unwrap_err();
            assert!(error.contains("scheme"), "unexpected error: {error}");
        }
    }

    #[test]
    fn add_stdio_server_splits_the_command() {
        let mut config = McpConfig::new();
        config
            .add_stdio_server(
                "filesystem",
                "npx -y @modelcontextprotocol/server-filesystem /tmp",
            )
            .unwrap();

        let server = config.get_server("filesystem").unwrap();
        assert_eq!(server.transport, McpTransport::Stdio);
        assert_eq!(server.command, "npx");
        assert_eq!(
            server.args,
            vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-filesystem".to_string(),
                "/tmp".to_string()
            ]
        );
        assert!(server.url.is_none(), "stdio must not carry a URL");
        assert!(server.headers.is_empty());
    }

    #[test]
    fn add_stdio_server_rejects_an_empty_command() {
        for command in ["", "   ", "\t\n"] {
            let mut config = McpConfig::new();
            let error = config.add_stdio_server("bad", command).unwrap_err();
            assert!(
                error.contains("non-empty command"),
                "unexpected error: {error}"
            );
            assert!(config.get_server("bad").is_none());
        }
    }

    #[test]
    fn load_nonexistent_file_errors() {
        let result = McpConfig::load(Path::new("/nonexistent/.mcp.json"));
        assert!(result.is_err());
    }

    #[test]
    fn default_path() {
        let cwd = Path::new("/some/dir");
        let path = McpConfig::default_path(cwd);
        assert_eq!(path, PathBuf::from("/some/dir/.mcp.json"));
    }
}
