//! CurrentDir tool — return the current working directory.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Return the current working directory as an absolute path.
#[derive(Debug)]
pub struct CurrentDirTool;

#[async_trait]
impl Tool for CurrentDirTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "CurrentDir".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "cwd": { "type": "string" }
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
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            output: ctx.cwd.to_string_lossy().to_string(),
            is_display: true,
            images: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::path::Path;

    #[tokio::test]
    async fn current_dir_returns_context_cwd() {
        let output = CurrentDirTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "CurrentDir".into(),
                    arguments: serde_json::json!({}),
                },
                ToolContext::new(ExecutionContext::Normal, Path::new("/tmp/demo")),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "/tmp/demo");
    }
}
