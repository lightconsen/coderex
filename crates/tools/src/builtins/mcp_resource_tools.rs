//! ListMcpResources / ReadMcpResource baseline tools.
//!
//! These tools drive an injected `McpResourceHandle` that the runtime
//! wires up from the `coderex-mcp` crate. Kept behind a small trait to
//! avoid a cross-crate dependency from `tools` -> `mcp`.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Shared-state key for an injected `McpResourceHandle`.
pub const MCP_RESOURCE_HANDLE_KEY: &str = "mcp_resource_handle";

/// An MCP resource descriptor returned by the handle.
#[derive(Debug, Clone)]
pub struct McpResourceEntry {
    /// Resource URI.
    pub uri: String,
    /// Human-readable name.
    pub name: String,
    /// MIME type.
    pub mime_type: String,
}

/// Abstracts MCP resource discovery + reads for this tool layer.
pub trait McpResourceHandle: Send + Sync {
    /// List all known MCP resources.
    fn list(&self) -> Vec<McpResourceEntry>;
    /// Read a resource by URI. Returns text content when available.
    fn read(&self, uri: &str) -> Result<String, String>;
}

fn get_handle(ctx: &ToolContext<'_>) -> Result<std::sync::Arc<dyn McpResourceHandle>, ToolError> {
    ctx.extra
        .get(MCP_RESOURCE_HANDLE_KEY)
        .and_then(|v| v.downcast_ref::<std::sync::Arc<dyn McpResourceHandle>>())
        .cloned()
        .ok_or_else(|| ToolError::InvalidInput {
            message: "MCP resource handle not available in this context".into(),
        })
}

/// List MCP resources discovered across connected servers.
#[derive(Debug)]
pub struct ListMcpResourcesTool;

#[async_trait]
impl Tool for ListMcpResourcesTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "ListMcpResources".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({ "type": "object", "properties": {} }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "resources": {
                        "type": "array",
                        "items": { "type": "object" }
                    }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        _input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let handle = get_handle(&ctx)?;
        let entries = handle.list();
        let json: Vec<_> = entries
            .into_iter()
            .map(|r| serde_json::json!({ "uri": r.uri, "name": r.name, "mime_type": r.mime_type }))
            .collect();
        Ok(ToolOutput {
            output: serde_json::to_string_pretty(&json).unwrap_or_else(|_| "[]".into()),
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Read a specific MCP resource by URI.
#[derive(Debug)]
pub struct ReadMcpResourceTool;

#[async_trait]
impl Tool for ReadMcpResourceTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "ReadMcpResource".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["uri"],
                "properties": { "uri": { "type": "string" } }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": { "content": { "type": "string" } }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let uri = input
            .arguments
            .get("uri")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput {
                message: "missing required field: uri".into(),
            })?;
        let handle = get_handle(&ctx)?;
        let content = handle
            .read(uri)
            .map_err(|e| ToolError::InvalidInput { message: e })?;
        Ok(ToolOutput {
            output: content,
            is_display: true,
            images: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    struct StubHandle;
    impl McpResourceHandle for StubHandle {
        fn list(&self) -> Vec<McpResourceEntry> {
            vec![McpResourceEntry {
                uri: "file:///x".into(),
                name: "x".into(),
                mime_type: "text/plain".into(),
            }]
        }
        fn read(&self, uri: &str) -> Result<String, String> {
            if uri == "file:///x" {
                Ok("hello".into())
            } else {
                Err("not found".into())
            }
        }
    }

    fn ctx_with_handle() -> ToolContext<'static> {
        let handle: Arc<dyn McpResourceHandle> = Arc::new(StubHandle);
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(MCP_RESOURCE_HANDLE_KEY.into(), Box::new(handle));
        ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        }
    }

    #[tokio::test]
    async fn list_resources_returns_entries() {
        let out = ListMcpResourcesTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "ListMcpResources".into(),
                    arguments: serde_json::json!({}),
                },
                ctx_with_handle(),
            )
            .await
            .unwrap();
        assert!(out.output.contains("file:///x"));
    }

    #[tokio::test]
    async fn read_resource_returns_content() {
        let out = ReadMcpResourceTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "ReadMcpResource".into(),
                    arguments: serde_json::json!({"uri": "file:///x"}),
                },
                ctx_with_handle(),
            )
            .await
            .unwrap();
        assert_eq!(out.output, "hello");
    }

    #[tokio::test]
    async fn read_missing_uri_errors() {
        let err = ReadMcpResourceTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "ReadMcpResource".into(),
                    arguments: serde_json::json!({}),
                },
                ctx_with_handle(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }
}
