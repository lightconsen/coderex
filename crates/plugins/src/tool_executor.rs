//! Plugin tool executor — implements `coderex_core::tool_executor::ToolExecutor`.
//!
//! Dispatches tool calls to plugin processes via `PluginManager`.

use crate::plugins::PluginManager;
use coderex_core::tool_executor::{ToolDispatchOutcome, ToolExecutor};
use coderex_core::types::ToolResult;
use serde_json::Value;
use std::sync::Arc;

/// A `ToolExecutor` implementation that routes calls to plugin processes.
#[derive(Debug, Clone)]
pub struct PluginToolExecutor {
    manager: Arc<PluginManager>,
}

impl PluginToolExecutor {
    /// Create a new plugin tool executor backed by the given manager.
    #[must_use]
    pub const fn new(manager: Arc<PluginManager>) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for PluginToolExecutor {
    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &Value,
    ) -> ToolDispatchOutcome {
        let Some(plugin_name) = self.manager.find_plugin_for_command(tool_name) else {
            return ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: format!("Plugin tool '{tool_name}' not found"),
                    is_error: true,
                    images: Vec::new(),
                },
            };
        };

        // Convert JSON input to string arguments for the plugin command.
        let args = input.as_array().map_or_else(
            || vec![input.to_string()],
            |arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            },
        );

        match self
            .manager
            .execute_command(&plugin_name, tool_name, &args)
            .await
        {
            Ok(output) => ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output,
                    is_error: false,
                    images: Vec::new(),
                },
            },
            Err(e) => ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: format!("Plugin tool '{tool_name}' failed: {e}"),
                    is_error: true,
                    images: Vec::new(),
                },
            },
        }
    }
}
