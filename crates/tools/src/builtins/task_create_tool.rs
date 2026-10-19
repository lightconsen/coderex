//! TaskCreate tool — create a structured task.

use crate::context::{ToolError, ToolOutput};
use crate::task_store::{TASK_STORE_KEY, TaskRecord};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;

/// Create a new structured task.
#[derive(Debug)]
pub struct TaskCreateTool;

#[async_trait]
impl Tool for TaskCreateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "TaskCreate".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["subject", "description"],
                "properties": {
                    "subject": { "type": "string", "description": "Brief task title" },
                    "description": { "type": "string", "description": "Detailed task description" },
                    "activeForm": { "type": "string", "description": "Present continuous form for spinner" },
                    "metadata": { "type": "object", "description": "Arbitrary metadata" },
                    "blocks": { "type": "array", "items": { "type": "string" }, "description": "Task IDs this task blocks" },
                    "blockedBy": { "type": "array", "items": { "type": "string" }, "description": "Task IDs that must complete first" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "taskId": { "type": "string" }
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
        let subject = extract_string(&input.arguments, "subject")?;
        let description = extract_string(&input.arguments, "description")?;
        let active_form = extract_string_opt(&input.arguments, "activeForm");
        let metadata = input.arguments.get("metadata").cloned();
        let blocks = extract_string_array(&input.arguments, "blocks");
        let blocked_by = extract_string_array(&input.arguments, "blockedBy");

        let store = get_store(&ctx).ok_or_else(|| ToolError::InvalidInput {
            message: "task store not available".into(),
        })?;

        let id = store.next_id();
        let mut record = TaskRecord::new(&id, &subject);
        record.description = description;
        record.active_form = active_form.unwrap_or_default();
        record.metadata = metadata;
        record.blocks = blocks;
        record.blocked_by = blocked_by;

        store.create(record);

        Ok(ToolOutput {
            output: id,
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

fn extract_string_array(args: &serde_json::Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::task_store::TaskStore;
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        let store = Arc::new(TaskStore::new());
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(TASK_STORE_KEY.to_string(), Box::new(store));
        ToolContext {
            elicit_tx: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        }
    }

    fn task_create_input(subject: &str, description: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "TaskCreate".into(),
            arguments: serde_json::json!({ "subject": subject, "description": description }),
        }
    }

    #[tokio::test]
    async fn task_create_returns_id() {
        let output = TaskCreateTool
            .execute(task_create_input("test", "do something"), test_ctx())
            .await
            .unwrap();
        assert_eq!(output.output, "1");
    }

    #[tokio::test]
    async fn task_create_missing_required_field() {
        let input = ToolInput {
            call_id: "1".into(),
            name: "TaskCreate".into(),
            arguments: serde_json::json!({ "subject": "test" }),
        };
        let result = TaskCreateTool.execute(input, test_ctx()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn task_create_without_store_errors() {
        let ctx = ToolContext::new(ExecutionContext::Normal, Path::new("/tmp"));
        let result = TaskCreateTool
            .execute(task_create_input("test", "desc"), ctx)
            .await;
        assert!(result.is_err());
    }
}
