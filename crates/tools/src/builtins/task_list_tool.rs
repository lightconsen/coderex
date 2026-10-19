//! TaskList tool — list all tasks.

use crate::context::{ToolError, ToolOutput};
use crate::task_store::{TASK_STORE_KEY, TaskStatus};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;

/// List all tasks, optionally filtered by status.
#[derive(Debug)]
pub struct TaskListTool;

#[async_trait]
impl Tool for TaskListTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "TaskList".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "deleted"], "description": "Filter by status" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tasks": { "type": "array" }
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
        let store = get_store(&ctx).ok_or_else(|| ToolError::InvalidInput {
            message: "task store not available".into(),
        })?;

        let status_filter =
            extract_string_opt(&input.arguments, "status").map(|s| match s.as_str() {
                "pending" => TaskStatus::Pending,
                "in_progress" => TaskStatus::InProgress,
                "completed" => TaskStatus::Completed,
                "deleted" => TaskStatus::Deleted,
                _ => TaskStatus::Pending,
            });

        let tasks = store.list(status_filter);
        let output = serde_json::to_string_pretty(&tasks).unwrap_or_else(|_| "[]".into());

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

fn extract_string_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(String::from)
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
        let mut r1 = TaskRecord::new("1", "task1");
        r1.status = TaskStatus::Pending;
        let mut r2 = TaskRecord::new("2", "task2");
        r2.status = TaskStatus::Completed;
        store.create(r1);
        store.create(r2);
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
    async fn task_list_returns_all() {
        let output = TaskListTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "TaskList".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("task1"));
        assert!(output.output.contains("task2"));
    }

    #[tokio::test]
    async fn task_list_filters_by_status() {
        let output = TaskListTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "TaskList".into(),
                    arguments: serde_json::json!({ "status": "completed" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("task2"));
        assert!(!output.output.contains("task1"));
    }
}
