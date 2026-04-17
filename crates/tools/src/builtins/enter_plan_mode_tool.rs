//! EnterPlanMode tool — signal to enter plan mode.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Use this tool proactively when you're about to start a non-trivial implementation task.
#[derive(Debug)]
pub struct EnterPlanModeTool;

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "EnterPlanMode".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "entered": { "type": "boolean" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, _input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        // Actual mode change is session-level; this is a no-op compatibility layer.
        Ok(ToolOutput {
            output: "true".to_string(),
            is_display: true,
        })
    }
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
    async fn enter_plan_mode_returns_success() {
        let output = EnterPlanModeTool
            .execute(
                ToolInput { call_id: "1".into(), name: "EnterPlanMode".into(), arguments: serde_json::json!({}) },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "true");
    }
}
