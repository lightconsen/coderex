//! McpAuth tool — trigger MCP OAuth authentication flow.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Trigger MCP OAuth authentication flow for a server.
#[derive(Debug)]
pub struct McpAuthTool;

#[async_trait]
impl Tool for McpAuthTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "McpAuth".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["server_url"],
                "properties": {
                    "server_url": { "type": "string", "description": "MCP server URL to authenticate with" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "authenticated": { "type": "boolean" },
                    "auth_url": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let server_url = extract_string(&input.arguments, "server_url")?;

        // In a real implementation, this would trigger the MCP OAuth flow
        // by opening a browser or returning an auth URL.
        Ok(ToolOutput {
            output: serde_json::json!({
                "authenticated": false,
                "auth_url": format!("{server_url}/oauth/authorize"),
                "note": "visit auth_url to complete authentication"
            }).to_string(),
            is_display: true,
        })
    }
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use crate::context::ExecutionContext;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext {
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn mcp_auth_returns_auth_url() {
        let output = McpAuthTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "McpAuth".into(),
                    arguments: serde_json::json!({ "server_url": "https://example.com" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("auth_url"));
    }

    #[tokio::test]
    async fn mcp_auth_missing_url() {
        let result = McpAuthTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "McpAuth".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
