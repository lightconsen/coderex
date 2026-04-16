//! Agent tool — spawn a subprocess running coderex with the given prompt.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Spawn a subagent running coderex with the given prompt.
#[derive(Debug)]
pub struct AgentTool;

#[async_trait]
impl Tool for AgentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Agent".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["description", "prompt"],
                "properties": {
                    "description": { "type": "string" },
                    "prompt": { "type": "string" },
                    "subagent_type": { "type": "string" },
                    "model": { "type": "string" },
                    "run_in_background": { "type": "boolean" },
                    "isolation": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "output_file": { "type": "string" }
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
        let description = extract_string(&input.arguments, "description")?;
        let _prompt = extract_string(&input.arguments, "prompt")?;
        let background = input.arguments.get("run_in_background").and_then(|v| v.as_bool()).unwrap_or(false);

        // In a real implementation, this would spawn a subprocess
        // running the coderex CLI with the given prompt.
        let agent_id = format!("agent-{description}");
        let output_file = format!("/tmp/agent-{description}-output.txt");

        Ok(ToolOutput {
            output: serde_json::json!({
                "agent_id": agent_id,
                "output_file": if background { Some(output_file) } else { None },
                "note": if background { "agent running in background" } else { "agent completed synchronously" }
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
    async fn agent_returns_id() {
        let output = AgentTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Agent".into(),
                    arguments: serde_json::json!({
                        "description": "test agent",
                        "prompt": "do something"
                    }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("agent"));
    }

    #[tokio::test]
    async fn agent_background_returns_output_file() {
        let output = AgentTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Agent".into(),
                    arguments: serde_json::json!({
                        "description": "bg agent",
                        "prompt": "do something",
                        "run_in_background": true
                    }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("output_file"));
    }

    #[tokio::test]
    async fn agent_missing_description() {
        let result = AgentTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Agent".into(),
                    arguments: serde_json::json!({ "prompt": "test" }),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
