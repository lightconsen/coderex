//! MCP tool executor — implements `coderex_core::tool_executor::ToolExecutor`.
//!
//! Dispatches tool calls to MCP servers via `McpManager`.

use crate::manager::McpManager;
use crate::result_adapter::McpResultAdapter;
use coderex_core::tool_executor::{ToolDispatchOutcome, ToolExecutor};
use coderex_core::types::ToolResult;
use serde_json::Value;
use std::sync::Arc;

/// A `ToolExecutor` implementation that routes calls to MCP servers.
#[derive(Debug, Clone)]
pub struct McpToolExecutor {
    manager: Arc<McpManager>,
}

impl McpToolExecutor {
    /// Create a new MCP tool executor backed by the given manager.
    #[must_use]
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for McpToolExecutor {
    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &Value,
    ) -> ToolDispatchOutcome {
        let proxy = match self.manager.get_tool_proxy(tool_name) {
            Some(p) => p,
            None => {
                return ToolDispatchOutcome {
                    result: ToolResult {
                        tool_call_id: tool_call_id.to_string(),
                        output: format!("MCP tool '{tool_name}' not found"),
                        is_error: true,
                    },
                };
            }
        };

        match proxy.call(input.clone()).await {
            Ok(result) => ToolDispatchOutcome {
                result: McpResultAdapter::adapt(tool_call_id, &result),
            },
            Err(e) => ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: format!("MCP tool '{tool_name}' failed: {e}"),
                    is_error: true,
                },
            },
        }
    }
}

/// Composite tool executor that tries the primary executor first,
/// then falls back to MCP.
#[derive(Clone)]
pub struct CompositeToolExecutor {
    primary: Arc<dyn ToolExecutor>,
    mcp: Arc<McpToolExecutor>,
}

impl std::fmt::Debug for CompositeToolExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeToolExecutor")
            .field("has_primary", &true)
            .field("has_mcp", &true)
            .finish()
    }
}

impl CompositeToolExecutor {
    /// Create a composite that tries `primary` first, then `mcp`.
    #[must_use]
    pub fn new(primary: Arc<dyn ToolExecutor>, mcp: Arc<McpToolExecutor>) -> Self {
        Self { primary, mcp }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for CompositeToolExecutor {
    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &Value,
    ) -> ToolDispatchOutcome {
        // Try primary first
        let primary_result = self.primary.execute(tool_call_id, tool_name, input).await;

        // If primary returned "not found", try MCP fallback
        let is_not_found = primary_result.result.is_error
            && (primary_result.result.output.contains("not found")
                || primary_result.result.output.contains("not registered"));

        if is_not_found {
            self.mcp.execute(tool_call_id, tool_name, input).await
        } else {
            primary_result
        }
    }
}
