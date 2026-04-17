//! TeamDelete tool — delete an existing team (baseline tool).

use crate::builtins::team_create_tool::{TeamManagerHandle, TEAM_MANAGER_KEY};
use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Delete a team.
#[derive(Debug)]
pub struct TeamDeleteTool;

#[async_trait]
impl Tool for TeamDeleteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "TeamDelete".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["team_id"],
                "properties": { "team_id": { "type": "string" } }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": { "deleted": { "type": "boolean" } }
            }),
            requires_approval: true,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let team_id = input
            .arguments
            .get("team_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput {
                message: "missing required field: team_id".into(),
            })?;

        let handle = ctx
            .extra
            .get(TEAM_MANAGER_KEY)
            .and_then(|v| v.downcast_ref::<std::sync::Arc<dyn TeamManagerHandle>>())
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput {
                message: "team manager not available in this context".into(),
            })?;

        handle
            .delete_team(team_id)
            .map_err(|e| ToolError::InvalidInput { message: e })?;

        Ok(ToolOutput {
            output: format!("deleted {team_id}"),
            is_display: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::team_create_tool::TeamManagerHandle;
    use crate::context::ExecutionContext;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    struct StubManager;
    impl TeamManagerHandle for StubManager {
        fn create_team(&self, _name: &str) -> Result<String, String> { Ok("x".into()) }
        fn delete_team(&self, _id: &str) -> Result<(), String> { Ok(()) }
    }

    #[tokio::test]
    async fn team_delete_succeeds() {
        let handle: Arc<dyn TeamManagerHandle> = Arc::new(StubManager);
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(TEAM_MANAGER_KEY.into(), Box::new(handle));
        let ctx = ToolContext {
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        };
        let out = TeamDeleteTool.execute(
            ToolInput { call_id: "1".into(), name: "TeamDelete".into(), arguments: serde_json::json!({"team_id": "team_1"}) },
            ctx,
        ).await.unwrap();
        assert!(out.output.contains("team_1"));
    }
}
