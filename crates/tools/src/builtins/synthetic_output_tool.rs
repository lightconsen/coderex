//! SyntheticOutput tool — format internal output into REPL-consumable format.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Format tool output for consumption by the REPL interface.
#[derive(Debug)]
pub struct SyntheticOutputTool;

#[async_trait]
impl Tool for SyntheticOutputTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "SyntheticOutput".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["content"],
                "properties": {
                    "content": { "type": "string", "description": "Raw content to format" },
                    "format": { "type": "string", "enum": ["plain", "json", "display"], "description": "Output format" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "formatted": { "type": "string" }
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
        input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let content = extract_string(&input.arguments, "content")?;
        let format =
            extract_string_opt(&input.arguments, "format").unwrap_or_else(|| "plain".into());

        let formatted = match format.as_str() {
            "json" => serde_json::json!({ "content": content }).to_string(),
            "display" => format!("=== Output ===\n{content}\n=== End ==="),
            _ => content.clone(),
        };

        Ok(ToolOutput {
            output: formatted,
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

fn extract_string_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(String::from)
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
    async fn synthetic_output_plain() {
        let output = SyntheticOutputTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "SyntheticOutput".into(),
                    arguments: serde_json::json!({ "content": "hello" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "hello");
    }

    #[tokio::test]
    async fn synthetic_output_json() {
        let output = SyntheticOutputTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "SyntheticOutput".into(),
                    arguments: serde_json::json!({ "content": "hello", "format": "json" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("hello"));
        assert!(output.output.contains("content"));
    }
}
