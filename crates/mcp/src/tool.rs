//! MCP tools as ordinary `Tool` implementations.
//!
//! A discovered MCP tool is registered in the tool registry under its
//! server-qualified name, so the engine dispatches it through the same
//! dispatcher and executor as a built-in tool — one execution path, with the
//! same timeout, output limit, and concurrency rules.

use crate::manager::McpManager;
use crate::proxy::McpToolInfo;
use crate::result_adapter::McpResultAdapter;
use coderex_tools::{
    InterruptBehavior, Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolOutput,
    ToolTier,
};
use std::sync::Arc;

/// A discovered MCP tool, exposed to the engine as a `Tool`.
#[derive(Debug)]
pub struct McpTool {
    info: McpToolInfo,
    manager: Arc<McpManager>,
}

impl McpTool {
    /// Wrap a discovered tool. The connection is resolved at call time, so a
    /// server that reconnects (or is not up yet) still works.
    #[must_use]
    pub const fn new(info: McpToolInfo, manager: Arc<McpManager>) -> Self {
        Self { info, manager }
    }

    /// The public, server-qualified name this tool is registered under.
    #[must_use]
    pub fn qualified_name(&self) -> String {
        self.info.qualified_name()
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.info.qualified_name(),
            tier: ToolTier::DynamicMcp,
            input_schema: self.info.input_schema.clone(),
            output_schema: serde_json::json!({}),
            requires_approval: false,
            is_enabled: true,
            // MCP tool annotations (`readOnlyHint`, `destructiveHint`) are not
            // parsed yet. Until they are, assume the worst on both counts:
            // nothing an MCP tool returns is cached, and plan mode (read-only
            // tools only) does not offer it.
            is_read_only: false,
            // Calls to one server serialise on its JSON-RPC channel, so two
            // tools on the same server never overlap regardless.
            is_concurrency_safe: true,
            interrupt_behavior: InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let name = self.info.qualified_name();
        let proxy = self
            .manager
            .get_tool_proxy(&name)
            .ok_or_else(|| ToolError::Execution {
                name: name.clone(),
                message: format!("MCP server '{}' is not connected", self.info.server),
            })?;

        let result = proxy
            .call(input.arguments)
            .await
            .map_err(|e| ToolError::Execution {
                name: name.clone(),
                message: e,
            })?;

        // `tools/call` reports tool-level failures in-band (`isError`), so the
        // adapted result keeps its error flag rather than becoming a `ToolError`.
        let adapted = McpResultAdapter::adapt(&input.call_id, &result);
        if adapted.is_error {
            return Err(ToolError::Execution {
                name,
                message: adapted.output,
            });
        }

        Ok(ToolOutput {
            output: adapted.output,
            is_display: false,
            images: adapted.images,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> McpToolInfo {
        McpToolInfo {
            server: "filesystem".into(),
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn descriptor_is_registered_under_the_qualified_name() {
        let tool = McpTool::new(info(), Arc::new(McpManager::new()));
        let desc = tool.descriptor();

        assert_eq!(desc.name, "mcp__filesystem__read_file");
        assert_eq!(desc.tier, ToolTier::DynamicMcp);
        assert!(!desc.is_read_only);
        assert!(desc.is_enabled);
    }

    #[tokio::test]
    async fn execute_without_a_connection_reports_the_server() {
        let tool = McpTool::new(info(), Arc::new(McpManager::new()));
        let err = tool
            .execute(
                ToolInput {
                    call_id: "call-1".into(),
                    name: "mcp__filesystem__read_file".into(),
                    arguments: serde_json::json!({}),
                },
                ToolContext::new(
                    coderex_tools::ExecutionContext::Normal,
                    std::path::Path::new("/tmp"),
                ),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(&err, ToolError::Execution { name, message }
                if name == "mcp__filesystem__read_file" && message.contains("filesystem")),
            "unexpected error: {err}"
        );
    }
}
