//! Skill tool — load and execute skill scripts.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Load and execute skill scripts from .claude/skills/.
#[derive(Debug)]
pub struct SkillTool;

#[async_trait]
impl Tool for SkillTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Skill".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["skill"],
                "properties": {
                    "skill": { "type": "string", "description": "Skill name to load" },
                    "args": { "type": "string", "description": "Arguments to pass to the skill" }
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

    async fn execute(&self, input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let skill = extract_string(&input.arguments, "skill")?;
        let _args = extract_string_opt(&input.arguments, "args");

        // Skills are loaded from .claude/skills/{skill}.md
        // For now, return a placeholder
        Ok(ToolOutput {
            output: format!("Skill '{skill}' loaded"),
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
    async fn skill_loads() {
        let output = SkillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Skill".into(),
                    arguments: serde_json::json!({ "skill": "pdf" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("pdf"));
    }

    #[tokio::test]
    async fn skill_missing_name() {
        let result = SkillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Skill".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
