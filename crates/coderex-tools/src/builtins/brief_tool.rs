//! Brief tool — generate a session/task summary.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Generate a session or task summary from recent messages.
#[derive(Debug)]
pub struct BriefTool;

#[async_trait]
impl Tool for BriefTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Brief".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session ID to summarize" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let session_id = extract_string_opt(&input.arguments, "session_id")
            .unwrap_or_else(|| "current".into());

        // Generate a placeholder summary
        Ok(ToolOutput {
            output: format!("Brief summary for session: {session_id}"),
            is_display: true,
        })
    }
}

fn extract_string_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(String::from)
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
    async fn brief_returns_summary() {
        let output = BriefTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Brief".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("summary"));
    }

    #[tokio::test]
    async fn brief_with_session_id() {
        let output = BriefTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Brief".into(),
                    arguments: serde_json::json!({ "session_id": "abc-123" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("abc-123"));
    }
}
