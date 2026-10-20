//! TaskUpdate tool — update a task's status or description.

use crate::context::{ToolError, ToolOutput};
use crate::task_store::{TASK_STORE_KEY, TaskStatus};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;

/// Update task status, subject, or description.
#[derive(Debug)]
pub struct TaskUpdateTool;

#[async_trait]
impl Tool for TaskUpdateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "TaskUpdate".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["taskId"],
                "properties": {
                    "taskId": { "type": "string" },
                    "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "deleted"] },
                    "subject": { "type": "string" },
                    "description": { "type": "string" },
                    "activeForm": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "updated": { "type": "boolean" }
                }
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
        let task_id = extract_string(&input.arguments, "taskId")?;
        let status = extract_string_opt(&input.arguments, "status").map(|s| match s.as_str() {
            "in_progress" => TaskStatus::InProgress,
            "completed" => TaskStatus::Completed,
            "deleted" => TaskStatus::Deleted,
            _ => TaskStatus::Pending,
        });
        let subject = extract_string_opt(&input.arguments, "subject");
        let description = extract_string_opt(&input.arguments, "description");
        let active_form = extract_string_opt(&input.arguments, "activeForm");

        let store = get_store(&ctx).ok_or_else(|| ToolError::InvalidInput {
            message: "task store not available".into(),
        })?;

        let ok = store.update(&task_id, status, subject, description, active_form);

        Ok(ToolOutput {
            output: if ok { "true" } else { "false" }.to_string(),
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
        store.create(TaskRecord::new("1", "task1"));
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
    async fn task_update_changes_status() {
        let output = TaskUpdateTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "TaskUpdate".into(),
                    arguments: serde_json::json!({ "taskId": "1", "status": "completed" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "true");
    }

    #[tokio::test]
    async fn task_update_nonexistent_returns_false() {
        let output = TaskUpdateTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "TaskUpdate".into(),
                    arguments: serde_json::json!({ "taskId": "999", "status": "completed" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "false");
    }
}
