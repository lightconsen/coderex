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

        let marking_completed = status == Some(TaskStatus::Completed);
        let ok = store.update(&task_id, status, subject, description, active_form);

        // Completing a task is a lifecycle moment the engine never sees — it
        // happens inside this tool — so the tool fires `TaskCompleted`.
        if ok && marking_completed {
            let mut hook_context = std::collections::HashMap::new();
            hook_context.insert("task_id".to_string(), task_id.clone());
            crate::tool::fire_tool_hook(&ctx, "TaskCompleted", &task_id, &hook_context).await;
        }

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
    use coderex_core::hook_executor::{HookExecutor, HookFireResult};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex;

    fn test_ctx() -> ToolContext<'static> {
        let store = Arc::new(TaskStore::new());
        store.create(TaskRecord::new("1", "task1"));
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(TASK_STORE_KEY.to_string(), Box::new(store));
        ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        }
    }

    /// Records every hook the tool fires, so a test can assert on the event.
    #[derive(Default)]
    struct RecordingHooks {
        fired: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl HookExecutor for RecordingHooks {
        async fn fire_hook(
            &self,
            event_name: &str,
            _input: &str,
            _context: &HashMap<String, String>,
        ) -> HookFireResult {
            self.fired.lock().unwrap().push(event_name.to_string());
            HookFireResult::default()
        }
    }

    fn ctx_with_hooks() -> (ToolContext<'static>, Arc<RecordingHooks>) {
        let store = Arc::new(TaskStore::new());
        store.create(TaskRecord::new("1", "task1"));
        let hooks = Arc::new(RecordingHooks::default());
        let typed: Arc<dyn HookExecutor> = hooks.clone();
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(TASK_STORE_KEY.to_string(), Box::new(store));
        extra.insert(crate::tool::HOOK_EXECUTOR_KEY.to_string(), Box::new(typed));
        let ctx = ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        };
        (ctx, hooks)
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

    /// Completing a task is the lifecycle moment `TaskCompleted` names, and
    /// nothing else in the engine sees it — so the tool has to fire it.
    #[tokio::test]
    async fn completing_a_task_fires_task_completed() {
        let (ctx, hooks) = ctx_with_hooks();
        TaskUpdateTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "TaskUpdate".into(),
                    arguments: serde_json::json!({ "taskId": "1", "status": "completed" }),
                },
                ctx,
            )
            .await
            .unwrap();
        assert_eq!(*hooks.fired.lock().unwrap(), vec!["TaskCompleted"]);
    }

    /// An update that is not a completion is not a `TaskCompleted`.
    #[tokio::test]
    async fn a_non_completing_update_does_not_fire_task_completed() {
        let (ctx, hooks) = ctx_with_hooks();
        TaskUpdateTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "TaskUpdate".into(),
                    arguments: serde_json::json!({ "taskId": "1", "status": "in_progress" }),
                },
                ctx,
            )
            .await
            .unwrap();
        assert!(hooks.fired.lock().unwrap().is_empty());
    }

    /// A completion that changed nothing (no such task) must not fire.
    #[tokio::test]
    async fn a_failed_completion_does_not_fire_task_completed() {
        let (ctx, hooks) = ctx_with_hooks();
        TaskUpdateTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "TaskUpdate".into(),
                    arguments: serde_json::json!({ "taskId": "999", "status": "completed" }),
                },
                ctx,
            )
            .await
            .unwrap();
        assert!(hooks.fired.lock().unwrap().is_empty());
    }
}
