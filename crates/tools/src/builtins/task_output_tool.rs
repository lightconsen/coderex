//! TaskOutput tool — get a task's output file contents.

use crate::context::{ToolError, ToolOutput};
use crate::task_store::TASK_STORE_KEY;
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;

/// Get output from a task's output file.
#[derive(Debug)]
pub struct TaskOutputTool;

const MAX_OUTPUT_CHARS: usize = 10000;

#[async_trait]
impl Tool for TaskOutputTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "TaskOutput".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["taskId"],
                "properties": {
                    "taskId": { "type": "string" },
                    "block": { "type": "boolean", "description": "Wait for output file to exist" },
                    "timeout": { "type": "number", "description": "Max seconds to wait for output" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "output": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let task_id = extract_string(&input.arguments, "taskId")?;
        let block = input.arguments.get("block").and_then(|v| v.as_bool()).unwrap_or(false);
        let timeout_secs = input
            .arguments
            .get("timeout")
            .and_then(|v| v.as_f64())
            .unwrap_or(60.0);

        let store = get_store(&ctx).ok_or_else(|| ToolError::InvalidInput {
            message: "task store not available".into(),
        })?;

        let task = store.get(&task_id).ok_or_else(|| ToolError::InvalidInput {
            message: format!("task '{task_id}' not found"),
        })?;

        let output_path = task.output_file_path.ok_or_else(|| ToolError::InvalidInput {
            message: format!("task '{task_id}' has no output file"),
        })?;

        // If block mode, wait for file
        if block {
            let dur = std::time::Duration::from_secs_f64(timeout_secs);
            let start = std::time::Instant::now();
            while start.elapsed() < dur {
                if std::path::Path::new(&output_path).exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }

        let content = std::fs::read_to_string(&output_path).map_err(|e| ToolError::InvalidInput {
            message: format!("failed to read output file: {e}"),
        })?;

        let truncated = if content.len() > MAX_OUTPUT_CHARS {
            format!("{}\n\n... (truncated)", &content[..MAX_OUTPUT_CHARS])
        } else {
            content
        };

        Ok(ToolOutput {
            output: truncated,
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
#[allow(dead_code)]
mod tests {
    use super::*;
    use crate::task_store::{TaskRecord, TaskStore};
    use crate::tool::ToolContext;
    use crate::context::ExecutionContext;
    use std::collections::HashMap;
    use std::path::Path;

    fn test_ctx_with_output() -> ToolContext<'static> {
        let store = Arc::new(TaskStore::new());
        let mut r = TaskRecord::new("1", "task1");
        r.output_file_path = Some("/tmp/task_output.txt".into());
        store.create(r);
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
    async fn task_output_no_output_file() {
        let store = Arc::new(TaskStore::new());
        store.create(TaskRecord::new("2", "no output"));
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(TASK_STORE_KEY.to_string(), Box::new(store));
        let ctx = ToolContext {
            elicit_tx: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        };

        let result = TaskOutputTool
            .execute(
                ToolInput { call_id: "1".into(), name: "TaskOutput".into(), arguments: serde_json::json!({ "taskId": "2" }) },
                ctx,
            )
            .await;
        assert!(result.is_err());
    }
}
