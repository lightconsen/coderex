//! TodoWrite tool — create/update structured task list.

use crate::context::{ToolError, ToolOutput};
use crate::todo_store::{TODO_STORE_KEY, TodoItem, TodoStatus};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;

/// Create or update the structured task list.
#[derive(Debug)]
pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "TodoWrite".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["todos"],
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["subject", "status"],
                            "properties": {
                                "subject": { "type": "string" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                                "description": { "type": "string" },
                                "active": { "type": "boolean" }
                            }
                        }
                    }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "todos_written": { "type": "number" }
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
        let todos_value = input
            .arguments
            .get("todos")
            .ok_or_else(|| ToolError::InvalidInput {
                message: "missing required field: todos".into(),
            })?;

        let todos_raw: Vec<TodoInput> =
            serde_json::from_value(todos_value.clone()).map_err(|e| ToolError::InvalidInput {
                message: format!("invalid todos format: {e}"),
            })?;

        let todos: Vec<TodoItem> = todos_raw
            .into_iter()
            .map(|t| TodoItem {
                subject: t.subject,
                description: t.description.unwrap_or_default(),
                status: t.status,
                active: t.active.unwrap_or(false),
            })
            .collect();

        let count = todos.len();

        if let Some(store) = get_store(&ctx) {
            store.set_all(todos);
        }

        Ok(ToolOutput {
            output: format!("Wrote {count} todo(s)"),
            is_display: true,
        })
    }
}

#[derive(Deserialize)]
struct TodoInput {
    subject: String,
    status: TodoStatus,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    active: Option<bool>,
}

fn get_store(ctx: &ToolContext<'_>) -> Option<Arc<crate::todo_store::TodoStore>> {
    ctx.extra.get(TODO_STORE_KEY).and_then(|v| {
        v.downcast_ref::<Arc<crate::todo_store::TodoStore>>()
            .map(Arc::clone)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::todo_store::TodoStore;
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;

    fn test_ctx_with_store() -> ToolContext<'static> {
        let store = Arc::new(TodoStore::new());
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(TODO_STORE_KEY.to_string(), Box::new(Arc::new(store)));
        ToolContext {
            elicit_tx: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        }
    }

    fn todo_input(todos: serde_json::Value) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "TodoWrite".into(),
            arguments: serde_json::json!({ "todos": todos }),
        }
    }

    #[tokio::test]
    async fn todo_write_creates_todos() {
        let ctx = test_ctx_with_store();
        let todos = serde_json::json!([
            { "subject": "task1", "status": "pending" },
            { "subject": "task2", "status": "in_progress" }
        ]);

        let output = TodoWriteTool.execute(todo_input(todos), ctx).await.unwrap();
        assert!(output.output.contains("2 todo"));
    }

    #[tokio::test]
    async fn todo_write_empty_list() {
        let ctx = test_ctx_with_store();
        let output = TodoWriteTool
            .execute(todo_input(serde_json::json!([])), ctx)
            .await
            .unwrap();
        assert!(output.output.contains("0 todo"));
    }

    #[tokio::test]
    async fn todo_write_missing_field() {
        let input = ToolInput {
            call_id: "1".into(),
            name: "TodoWrite".into(),
            arguments: serde_json::json!({}),
        };
        let result = TodoWriteTool.execute(input, test_ctx_with_store()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn todo_write_without_store() {
        let ctx = ToolContext::new(ExecutionContext::Normal, Path::new("/tmp"));
        let todos = serde_json::json!([{ "subject": "task1", "status": "pending" }]);
        // Should not error even without a store
        let output = TodoWriteTool.execute(todo_input(todos), ctx).await.unwrap();
        assert!(output.output.contains("1 todo"));
    }
}
