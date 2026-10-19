//! TaskGet tool — get a single task by ID.

use crate::context::{ToolError, ToolOutput};
use crate::task_store::TASK_STORE_KEY;
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;

/// Get details for a single task.
#[derive(Debug)]
pub struct TaskGetTool;

#[async_trait]
impl Tool for TaskGetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "TaskGet".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["taskId"],
                "properties": {
                    "taskId": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": { "type": "object" }
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
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let task_id = extract_string(&input.arguments, "taskId")?;

        let store = get_store(&ctx).ok_or_else(|| ToolError::InvalidInput {
            message: "task store not available".into(),
        })?;

        let task = store.get(&task_id).ok_or_else(|| ToolError::InvalidInput {
            message: format!("task '{task_id}' not found"),
        })?;

        let output = serde_json::to_string_pretty(&task).map_err(|e| ToolError::InvalidInput {
            message: format!("failed to serialize task: {e}"),
        })?;

        Ok(ToolOutput {
            output,
            is_display: true,
            images: Vec::new(),
        })
    }
}

fn get_store(ctx: &ToolContext<'_>) -> Option<Arc<crate::task_store::TaskStore>> {
    ctx.extra.get(TASK_STORE_KEY).and_then(|v| {
        v.downcast_ref::<Arc<crate::task_store::TaskStore>>()
            .map(Arc::clone)
    })
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
    use crate::task_store::{TaskRecord, TaskStore};
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        let store = Arc::new(TaskStore::new());
        store.create(TaskRecord::new("1", "existing task"));
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(TASK_STORE_KEY.to_string(), Box::new(store));
        ToolContext {
            elicit_tx: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        }
    }

    #[tokio::test]
    async fn task_get_returns_task() {
        let output = TaskGetTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "TaskGet".into(),
                    arguments: serde_json::json!({ "taskId": "1" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("existing task"));
    }

    #[tokio::test]
    async fn task_get_not_found() {
        let result = TaskGetTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "TaskGet".into(),
                    arguments: serde_json::json!({ "taskId": "999" }),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
