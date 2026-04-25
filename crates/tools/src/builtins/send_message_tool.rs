//! SendMessage tool — send a message to a running agent/task.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Send a message to a running agent or task.
#[derive(Debug)]
pub struct SendMessageTool;

#[async_trait]
impl Tool for SendMessageTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "SendMessage".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["to", "message"],
                "properties": {
                    "to": { "type": "string", "description": "Target agent/task ID" },
                    "message": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "sent": { "type": "boolean" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let _to = extract_string(&input.arguments, "to")?;
        let _message = extract_string(&input.arguments, "message")?;

        // In a real implementation, this would write to a message file
        // or shared store that the target agent monitors.
        Ok(ToolOutput {
            output: "true".to_string(),
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
    async fn send_message_returns_true() {
        let output = SendMessageTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "SendMessage".into(),
                    arguments: serde_json::json!({ "to": "agent-1", "message": "hello" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "true");
    }

    #[tokio::test]
    async fn send_message_missing_fields() {
        let result = SendMessageTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "SendMessage".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
