//! TaskStop tool — stop a running task.

use crate::context::{ToolError, ToolOutput};
use crate::task_store::{TaskStatus, TASK_STORE_KEY};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;

/// Stop a running task.
#[derive(Debug)]
pub struct TaskStopTool;

#[async_trait]
impl Tool for TaskStopTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "TaskStop".into(),
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
                    "stopped": { "type": "boolean" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let task_id = extract_string(&input.arguments, "taskId")?;

        let store = get_store(&ctx).ok_or_else(|| ToolError::InvalidInput {
            message: "task store not available".into(),
        })?;

        let task = store.get(&task_id).ok_or_else(|| ToolError::InvalidInput {
            message: format!("task '{task_id}' not found"),
        })?;

        if matches!(task.status, TaskStatus::Completed | TaskStatus::Deleted) {
            return Ok(ToolOutput {
                output: "false".to_string(),
                is_display: true,
            });
        }

        let ok = store.update(&task_id, Some(TaskStatus::Deleted), None, None, None);

        Ok(ToolOutput {
            output: if ok { "true" } else { "false" }.to_string(),
            is_display: true,
        })
    }
}

fn get_store(ctx: &ToolContext<'_>) -> Option<Arc<crate::task_store::TaskStore>> {
    ctx.extra
        .get(TASK_STORE_KEY)
        .and_then(|v| v.downcast_ref::<Arc<crate::task_store::TaskStore>>().map(Arc::clone))
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
    use crate::task_store::{TaskRecord, TaskStore};
    use crate::tool::ToolContext;
    use crate::context::ExecutionContext;
    use std::collections::HashMap;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        let store = Arc::new(TaskStore::new());
        store.create(TaskRecord::new("1", "running task"));
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(TASK_STORE_KEY.to_string(), Box::new(store));
        ToolContext {
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        }
    }

    #[tokio::test]
    async fn task_stop_stops_task() {
        let output = TaskStopTool
            .execute(
                ToolInput { call_id: "1".into(), name: "TaskStop".into(), arguments: serde_json::json!({ "taskId": "1" }) },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "true");
    }

    #[tokio::test]
    async fn task_stop_not_found() {
        let result = TaskStopTool
            .execute(
                ToolInput { call_id: "1".into(), name: "TaskStop".into(), arguments: serde_json::json!({ "taskId": "999" }) },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
