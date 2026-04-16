//! RemoteTrigger tool — trigger remote Bridge work assignment.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;
use std::collections::HashMap;

/// Trigger remote Bridge work assignment.
#[derive(Debug)]
pub struct RemoteTriggerTool;

#[async_trait]
impl Tool for RemoteTriggerTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "RemoteTrigger".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["assignment_id"],
                "properties": {
                    "assignment_id": { "type": "string", "description": "Work assignment ID to trigger" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "triggered": { "type": "boolean" },
                    "status": { "type": "string" }
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
        let assignment_id = extract_string(&input.arguments, "assignment_id")?;

        // In a real implementation, this would make an HTTP request
        // to the Bridge service to trigger the work assignment.
        Ok(ToolOutput {
            output: serde_json::json!({
                "triggered": true,
                "status": format!("assignment {assignment_id} triggered")
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
    async fn remote_trigger_returns_triggered() {
        let output = RemoteTriggerTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "RemoteTrigger".into(),
                    arguments: serde_json::json!({ "assignment_id": "abc-123" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("triggered"));
    }

    #[tokio::test]
    async fn remote_trigger_missing_id() {
        let result = RemoteTriggerTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "RemoteTrigger".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
