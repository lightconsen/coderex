//! Built-in tools: EchoTool and NoOpTool.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use serde_json::Value;

/// Echo tool — returns its input arguments as text.
///
/// Useful for testing and debugging the tool pipeline.
#[derive(Debug)]
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "echo".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string"}
                },
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "echoed": {"type": "string"}
                },
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let echoed = match &input.arguments {
            Value::Object(map) => {
                if let Some(msg) = map.get("message").and_then(Value::as_str) {
                    msg.to_string()
                } else {
                    serde_json::to_string_pretty(&input.arguments)
                        .unwrap_or_else(|_| String::new())
                }
            }
            other => other.to_string(),
        };
        Ok(ToolOutput {
            output: echoed,
            is_display: true,
        })
    }
}

/// No-op tool — always returns empty success.
///
/// Useful as a placeholder or for testing permission/registry logic.
#[derive(Debug)]
pub struct NoOpTool;

#[async_trait]
impl Tool for NoOpTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "noop".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, _input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            output: String::new(),
            is_display: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext::new(
            crate::context::ExecutionContext::Normal,
            Path::new("/tmp"),
        )
    }

    #[tokio::test]
    async fn echo_tool_returns_input_message() {
        let output = EchoTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"message": "hello"}),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "hello");
        assert!(output.is_display);
    }

    #[tokio::test]
    async fn echo_tool_falls_back_to_json_when_no_message() {
        let output = EchoTool
            .execute(
                ToolInput {
                    call_id: "2".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"key": "value"}),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("key"));
        assert!(output.output.contains("value"));
    }

    #[tokio::test]
    async fn noop_tool_returns_empty_output() {
        let output = NoOpTool
            .execute(
                ToolInput {
                    call_id: "3".into(),
                    name: "noop".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.is_empty());
        assert!(!output.is_display);
    }

    #[test]
    fn echo_tool_descriptor_is_baseline() {
        let desc = EchoTool.descriptor();
        assert_eq!(desc.name, "echo");
        assert!(matches!(desc.tier, ToolTier::Baseline));
        assert!(!desc.is_read_only);
        assert!(desc.is_concurrency_safe);
    }

    #[test]
    fn noop_tool_descriptor_is_read_only() {
        let desc = NoOpTool.descriptor();
        assert_eq!(desc.name, "noop");
        assert!(desc.is_read_only);
        assert!(desc.is_concurrency_safe);
    }
}
