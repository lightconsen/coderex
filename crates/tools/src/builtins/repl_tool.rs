//! REPL tool — no-op compatibility layer for the REPL command interface.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// REPL command interface — no-op in the tool system.
#[derive(Debug)]
pub struct ReplTool;

#[async_trait]
impl Tool for ReplTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "REPL".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "result": { "type": "string" }
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
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        // REPL commands are handled at the session level, not as tools.
        Ok(ToolOutput {
            output: "REPL commands are handled by the session interface".to_string(),
            is_display: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext {
            elicit_tx: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn repl_returns_message() {
        let output = ReplTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "REPL".into(),
                    arguments: serde_json::json!({ "command": "/help" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("REPL"));
    }
}
