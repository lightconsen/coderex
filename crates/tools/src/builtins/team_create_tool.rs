//! TeamCreate tool — create a new team (baseline tool).

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Shared-state key under which a `TeamManagerHandle` may be stored
/// in `ToolContext.extra`. The handle itself is defined by the caller
/// (typically the runtime wiring this crate into a running app).
pub const TEAM_MANAGER_KEY: &str = "team_manager";

/// A callable team-manager facade that this tool can drive without
/// pulling `coderex-core` as a dependency (to avoid cycles).
pub trait TeamManagerHandle: Send + Sync {
    /// Create a team with the given name. Returns the new team id.
    fn create_team(&self, name: &str) -> Result<String, String>;
    /// Delete a team by id.
    fn delete_team(&self, team_id: &str) -> Result<(), String>;
}

/// Create a new team.
#[derive(Debug)]
pub struct TeamCreateTool;

#[async_trait]
impl Tool for TeamCreateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "TeamCreate".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": { "team_id": { "type": "string" } }
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
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let name = input
            .arguments
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput {
                message: "missing required field: name".into(),
            })?;

        let handle = ctx
            .extra
            .get(TEAM_MANAGER_KEY)
            .and_then(|v| v.downcast_ref::<std::sync::Arc<dyn TeamManagerHandle>>())
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput {
                message: "team manager not available in this context".into(),
            })?;

        let team_id = handle
            .create_team(name)
            .map_err(|e| ToolError::InvalidInput { message: e })?;

        Ok(ToolOutput {
            output: team_id,
            is_display: true,
            images: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    struct StubManager(Mutex<u32>);
    impl TeamManagerHandle for StubManager {
        fn create_team(&self, _name: &str) -> Result<String, String> {
            let mut n = self.0.lock().unwrap();
            *n += 1;
            Ok(format!("team_{}", *n))
        }
        fn delete_team(&self, _team_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn team_create_returns_id() {
        let handle: Arc<dyn TeamManagerHandle> = Arc::new(StubManager(Mutex::new(0)));
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(TEAM_MANAGER_KEY.into(), Box::new(handle));
        let ctx = ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        };
        let out = TeamCreateTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "TeamCreate".into(),
                    arguments: serde_json::json!({"name": "alpha"}),
                },
                ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.output, "team_1");
    }

    #[tokio::test]
    async fn team_create_missing_manager() {
        let ctx = ToolContext::new(ExecutionContext::Normal, Path::new("/tmp"));
        let err = TeamCreateTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "TeamCreate".into(),
                    arguments: serde_json::json!({"name": "a"}),
                },
                ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }
}
