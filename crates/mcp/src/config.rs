//! MCP configuration — `.mcp.json` parsing.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Configuration for a single MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// The command to run the server.
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
    #[serde(default)]
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
            },
        );
        config.add_server(
            "b",
            McpServerConfig {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
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
