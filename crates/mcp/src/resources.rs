//! MCP resource discovery and reading.

use serde::{Deserialize, Serialize};

/// An MCP resource (file, database row, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResource {
    /// Resource URI (e.g., "file:///path/to/file").
    pub uri: String,
    /// Human-readable name.
    pub name: String,
    /// MIME type.
    pub mime_type: String,
    /// Optional description.
    #[serde(default)]
    pub description: Option<String>,
}

/// The content read from an MCP resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResourceContent {
    /// The resource URI this content belongs to.
    pub uri: String,
    /// MIME type of the content.
    pub mime_type: String,
    /// Text content (for text resources).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Binary content as base64 (for binary resources).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// Resource registry — discovered resources from MCP servers.
#[derive(Debug, Default)]
pub struct McpResourceRegistry {
    resources: Vec<McpResource>,
}

impl McpResourceRegistry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self { resources: Vec::new() }
    }

    /// Register a discovered resource.
    pub fn register(&mut self, resource: McpResource) {
        self.resources.push(resource);
    }

    /// Find a resource by URI.
    #[must_use]
    pub fn get(&self, uri: &str) -> Option<&McpResource> {
        self.resources.iter().find(|r| r.uri == uri)
    }

    /// List all resources.
    #[must_use]
    pub fn list(&self) -> &[McpResource] {
        &self.resources
    }

    /// Filter resources by MIME type prefix.
    #[must_use]
    pub fn by_mime_prefix(&self, prefix: &str) -> Vec<&McpResource> {
        self.resources
            .iter()
            .filter(|r| r.mime_type.starts_with(prefix))
            .collect()
    }

    /// Number of resources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.resources.len()
    }

    /// Check if empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_resource() -> McpResource {
        McpResource {
            uri: "file:///tmp/test.txt".into(),
            name: "test.txt".into(),
            mime_type: "text/plain".into(),
            description: Some("A test file".into()),
        }
    }

    #[test]
    fn registry_starts_empty() {
        let reg = McpResourceRegistry::new();
        assert!(reg.is_empty());
    }

    #[test]
    fn register_and_list() {
        let mut reg = McpResourceRegistry::new();
        reg.register(sample_resource());
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.list()[0].name, "test.txt");
    }

    #[test]
    fn get_by_uri() {
        let mut reg = McpResourceRegistry::new();
        reg.register(sample_resource());
        let found = reg.get("file:///tmp/test.txt").unwrap();
        assert_eq!(found.description, Some("A test file".into()));
        assert!(reg.get("file:///nonexistent").is_none());
    }

    #[test]
    fn filter_by_mime_prefix() {
        let mut reg = McpResourceRegistry::new();
        reg.register(McpResource {
            uri: "file:///a.txt".into(), name: "a".into(),
            mime_type: "text/plain".into(), description: None,
        });
        reg.register(McpResource {
            uri: "file:///b.json".into(), name: "b".into(),
            mime_type: "application/json".into(), description: None,
        });
        reg.register(McpResource {
            uri: "file:///c.html".into(), name: "c".into(),
            mime_type: "text/html".into(), description: None,
        });
        let text = reg.by_mime_prefix("text/");
        assert_eq!(text.len(), 2);
        let json = reg.by_mime_prefix("application/");
        assert_eq!(json.len(), 1);
    }
}
