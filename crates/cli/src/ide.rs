//! IDE integration for `--ide`.
//!
//! CodeRex does not ship an IDE. What `--ide` does is connect to an *IDE MCP
//! server* — a process or endpoint the user configured — and make the tools it
//! exposes (typically `executeCode` and `getDiagnostics`) available to the
//! session. That is a real integration surface rather than a label: the tools
//! are discovered over MCP and registered into the same registry the model
//! dispatches through.
//!
//! Configuration is resolved, in order, from:
//!
//! 1. `CODEREX_IDE_MCP_URL` — an HTTP/SSE MCP endpoint.
//! 2. `CODEREX_IDE_MCP_COMMAND` — a stdio MCP server command line.
//! 3. `mcp_servers` / `mcpServers` in settings, under the `--ide` value or
//!    the name `ide`.
//! 4. `.mcp.json` (or `--mcp-config`), likewise.
//!
//! If none of those resolves, startup fails with the list of places checked.
//! If the server cannot be started or answers `tools/list` with nothing, that
//! is also a failure — `--ide` never becomes a silent no-op.

use anyhow::{Context, Result, bail};
use coderex_config::Settings;
use coderex_mcp::config::{McpConfig, McpServerConfig, McpTransport};
use coderex_mcp::manager::McpManager;
use std::path::Path;
use std::sync::Arc;

/// Environment variable for a stdio IDE MCP server,
/// e.g. `CODEREX_IDE_MCP_COMMAND="vscode-mcp --stdio"`.
pub const IDE_MCP_COMMAND_ENV: &str = "CODEREX_IDE_MCP_COMMAND";
/// Environment variable for a remote IDE MCP endpoint.
pub const IDE_MCP_URL_ENV: &str = "CODEREX_IDE_MCP_URL";
/// The conventional server name in settings and `.mcp.json`.
pub const DEFAULT_IDE_SERVER_NAME: &str = "ide";

/// A resolved IDE MCP server, before connecting.
#[derive(Debug, Clone)]
pub struct IdeServer {
    /// Name the server is registered under (and therefore tool prefix).
    pub name: String,
    /// Connection details.
    pub config: McpServerConfig,
    /// Where the configuration came from, for error and status messages.
    pub source: String,
}

/// A live IDE MCP connection.
#[derive(Debug)]
pub struct IdeConnection {
    /// Server name.
    pub name: String,
    /// Transport in use.
    pub transport: McpTransport,
    /// Qualified names of the tools the server exposes.
    pub tools: Vec<String>,
    /// The live manager, kept alive for the session so tool calls can resolve
    /// their connection.
    pub manager: Arc<McpManager>,
}

impl IdeConnection {
    /// One-line status suitable for logging or stderr.
    #[must_use]
    pub fn status_line(&self) -> String {
        let transport = match self.transport {
            McpTransport::Stdio => "stdio",
            McpTransport::Sse => "sse",
            McpTransport::Http => "http",
        };
        format!(
            "Connected to IDE MCP server '{}' over {transport}; {} tool(s): {}",
            self.name,
            self.tools.len(),
            self.tools.join(", ")
        )
    }
}

/// Resolve the configured IDE server.
///
/// # Errors
///
/// Returns a message naming every configuration source that was checked when
/// none of them provide a server for `flag_value`.
pub fn resolve_server(
    flag_value: &str,
    settings: Option<&Settings>,
    cwd: &Path,
    mcp_config_path: Option<&Path>,
) -> Result<IdeServer> {
    if let Some(url) = non_empty_env(IDE_MCP_URL_ENV) {
        return Ok(IdeServer {
            name: DEFAULT_IDE_SERVER_NAME.to_string(),
            config: McpServerConfig {
                transport: McpTransport::Http,
                url: Some(url),
                ..McpServerConfig::default()
            },
            source: IDE_MCP_URL_ENV.to_string(),
        });
    }

    if let Some(command) = non_empty_env(IDE_MCP_COMMAND_ENV) {
        let mut parts = command.split_whitespace();
        let program = parts.next().unwrap_or_default().to_string();
        if program.is_empty() {
            bail!("{IDE_MCP_COMMAND_ENV} is set but does not name a command");
        }
        return Ok(IdeServer {
            name: DEFAULT_IDE_SERVER_NAME.to_string(),
            config: McpServerConfig {
                transport: McpTransport::Stdio,
                command: program,
                args: parts.map(str::to_string).collect(),
                ..McpServerConfig::default()
            },
            source: IDE_MCP_COMMAND_ENV.to_string(),
        });
    }

    let names = candidate_names(flag_value);

    if let Some(config) = settings.and_then(|settings| settings.mcp_servers.as_ref()) {
        for name in &names {
            if let Some(server) = server_from_json(config, name) {
                return Ok(IdeServer {
                    name: (*name).to_string(),
                    config: server,
                    source: format!("settings mcp_servers.{name}"),
                });
            }
        }
    }

    let mcp_path = mcp_config_path.map_or_else(|| cwd.join(".mcp.json"), Path::to_path_buf);
    if mcp_path.exists() {
        match McpConfig::load(&mcp_path) {
            Ok(config) => {
                for name in &names {
                    if let Some(server) = config.get_server(name) {
                        return Ok(IdeServer {
                            name: (*name).to_string(),
                            config: server.clone(),
                            source: format!("{}.{name}", mcp_path.display()),
                        });
                    }
                }
            }
            Err(error) => {
                // A malformed file is a configuration error worth surfacing,
                // not something to skip on the way to "no IDE configured".
                return Err(error)
                    .with_context(|| format!("could not read MCP config {}", mcp_path.display()));
            }
        }
    }

    bail!(
        "--ide is set but no IDE MCP server is configured. Set {IDE_MCP_URL_ENV} or \
         {IDE_MCP_COMMAND_ENV}, add `mcpServers.ide` to settings, or add `ide` to {}. \
         (CodeRex has no bundled IDE; --ide connects to an MCP server.)",
        mcp_path.display()
    )
}

/// Connect, discover tools, and report status.
///
/// # Errors
///
/// Fails when the server process cannot be started, when it is not running
/// after startup, or when it exposes no tools.
pub async fn connect(server: IdeServer) -> Result<IdeConnection> {
    let manager = Arc::new(McpManager::new());
    let mut config = McpConfig::new();
    config.add_server(server.name.clone(), server.config.clone());
    manager.load_config(&config);
    manager.start_all().await;

    if !manager.is_running(&server.name) {
        bail!(
            "IDE MCP server '{}' (from {}) could not be started; check the configured \
             command or URL",
            server.name,
            server.source
        );
    }

    // `start_all` already asked for tools; repeat it so a server that was a
    // moment slow still gets a fair chance before we call the connection dead.
    manager.discover_all_tools().await;
    let tools: Vec<String> = manager
        .list_tool_info()
        .into_iter()
        .map(|tool| tool.qualified_name())
        .collect();

    if tools.is_empty() {
        bail!(
            "IDE MCP server '{}' (from {}) is running but exposed no tools; \
             `--ide` needs an MCP server that answers `tools/list` \
             (for example one exposing executeCode and getDiagnostics)",
            server.name,
            server.source
        );
    }

    Ok(IdeConnection {
        name: server.name,
        transport: server.config.transport,
        tools,
        manager,
    })
}

fn candidate_names(flag_value: &str) -> Vec<&str> {
    let flag_value = flag_value.trim();
    let mut names = Vec::new();
    if !flag_value.is_empty() && flag_value != DEFAULT_IDE_SERVER_NAME {
        names.push(flag_value);
    }
    names.push(DEFAULT_IDE_SERVER_NAME);
    names
}

/// Read `mcp_servers` / `mcpServers` from the settings JSON value.
fn server_from_json(value: &serde_json::Value, name: &str) -> Option<McpServerConfig> {
    let servers = value
        .get("mcpServers")
        .or_else(|| value.get("mcp_servers"))
        .unwrap_or(value);

    // A map of server name -> configuration is the documented shape. Fall back
    // to parsing the whole value as `McpConfig` so a value that is exactly
    // `{"ide": {...}}` still resolves.
    if let Some(entry) = servers.get(name) {
        if let Ok(config) = serde_json::from_value::<McpServerConfig>(entry.clone()) {
            return Some(config);
        }
    }
    serde_json::from_value::<McpConfig>(value.clone())
        .ok()
        .and_then(|config| config.get_server(name).cloned())
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// A stdio server configuration for a command and its arguments.
#[must_use]
pub fn stdio_server(name: impl Into<String>, command: impl Into<String>) -> IdeServer {
    IdeServer {
        name: name.into(),
        config: McpServerConfig {
            transport: McpTransport::Stdio,
            command: command.into(),
            ..McpServerConfig::default()
        },
        source: "explicit".into(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn mock_stdio_server(dir: &Path, tools: bool) -> PathBuf {
        let path = dir.join("mock-ide-mcp");
        let tools_response = if tools {
            r#"{"tools":[{"name":"executeCode","description":"Execute code in the IDE","inputSchema":{"type":"object"}},{"name":"getDiagnostics","description":"Read IDE diagnostics","inputSchema":{"type":"object"}}]}"#
        } else {
            r#"{"tools":[]}"#
        };
        let script = format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2024-11-05","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"mock-ide","version":"1.0"}}}}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{tools_response}}}\n' "$id"
      ;;
    *'"method":"resources/list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"resources":[]}}}}\n' "$id"
      ;;
  esac
done
"#
        );
        std::fs::write(&path, script).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[tokio::test]
    async fn connects_to_a_stdio_mcp_server_and_lists_its_tools() {
        let dir = tempfile::tempdir().unwrap();
        let path = mock_stdio_server(dir.path(), true);
        let server = stdio_server("ide", path.to_string_lossy().into_owned());

        let connection = connect(server)
            .await
            .expect("mock IDE server should connect");
        assert_eq!(connection.name, "ide");
        assert_eq!(connection.transport, McpTransport::Stdio);
        assert!(
            connection
                .tools
                .iter()
                .any(|name| name.ends_with("executeCode")),
            "{:?}",
            connection.tools
        );
        assert!(
            connection
                .tools
                .iter()
                .any(|name| name.ends_with("getDiagnostics")),
            "{:?}",
            connection.tools
        );
        let status = connection.status_line();
        assert!(
            status.contains("Connected to IDE MCP server 'ide'"),
            "{status}"
        );
        assert!(status.contains("2 tool(s)"), "{status}");
    }

    #[tokio::test]
    async fn a_server_that_exposes_no_tools_is_a_clear_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = mock_stdio_server(dir.path(), false);
        let server = stdio_server("ide", path.to_string_lossy().into_owned());

        let error = connect(server)
            .await
            .expect_err("empty tool list must fail");
        assert!(error.to_string().contains("exposed no tools"), "{error}");
    }

    #[tokio::test]
    async fn an_unreachable_server_is_a_clear_failure() {
        let server = stdio_server("ide", "/definitely/not/a/real/mcp-server");
        let error = connect(server)
            .await
            .expect_err("missing command must fail");
        assert!(
            error.to_string().contains("could not be started"),
            "{error}"
        );
    }

    #[test]
    fn resolves_a_server_from_settings() {
        let settings = Settings {
            mcp_servers: Some(json!({
                "ide": {
                    "type": "stdio",
                    "command": "/usr/local/bin/ide-mcp",
                    "args": ["--stdio"]
                }
            })),
            ..Settings::default()
        };

        let server = resolve_server("vscode", Some(&settings), Path::new("/work"), None)
            .expect("settings mcp_servers.ide should resolve");
        assert_eq!(server.name, "ide");
        assert_eq!(server.config.command, "/usr/local/bin/ide-mcp");
        assert_eq!(server.config.args, vec!["--stdio"]);
        assert!(server.source.contains("mcp_servers.ide"));
    }

    #[test]
    fn resolves_a_server_named_by_the_flag_value() {
        let settings = Settings {
            mcp_servers: Some(json!({
                "mcpServers": {
                    "vscode": { "command": "/bin/vscode-mcp", "args": [] }
                }
            })),
            ..Settings::default()
        };

        let server = resolve_server("vscode", Some(&settings), Path::new("/work"), None)
            .expect("the --ide value is tried as a server name");
        assert_eq!(server.name, "vscode");
        assert_eq!(server.config.command, "/bin/vscode-mcp");
    }

    #[test]
    fn a_missing_configuration_names_every_place_checked() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve_server("vscode", None, dir.path(), None)
            .expect_err("no configuration must fail");
        let rendered = error.to_string();
        assert!(rendered.contains(IDE_MCP_URL_ENV), "{rendered}");
        assert!(rendered.contains(IDE_MCP_COMMAND_ENV), "{rendered}");
        assert!(rendered.contains("mcpServers.ide"), "{rendered}");
        assert!(rendered.contains(".mcp.json"), "{rendered}");
    }

    #[test]
    fn malformed_mcp_config_is_reported_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        std::fs::write(&path, "{ not json").unwrap();

        let error = resolve_server("ide", None, dir.path(), Some(&path))
            .expect_err("malformed config must fail");
        assert!(
            error.to_string().contains("could not read MCP config"),
            "{error}"
        );
    }
}
