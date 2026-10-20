//! CronCreate tool — schedule a cron job.

use crate::context::{ToolError, ToolOutput};
use crate::cron_scheduler::{CRON_SCHEDULER_KEY, CronJob, CronScheduler};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;

/// Schedule a prompt to be enqueued at a future time.
#[derive(Debug)]
pub struct CronCreateTool;

#[async_trait]
impl Tool for CronCreateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "CronCreate".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["cron", "prompt"],
                "properties": {
                    "cron": { "type": "string", "description": "5-field cron expression" },
                    "prompt": { "type": "string" },
                    "recurring": { "type": "boolean" },
                    "durable": { "type": "boolean" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" }
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
        let cron = extract_string(&input.arguments, "cron")?;
        let prompt = extract_string(&input.arguments, "prompt")?;
        let recurring = input
            .arguments
            .get("recurring")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let durable = input
            .arguments
            .get("durable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let scheduler = get_scheduler(&ctx).ok_or_else(|| ToolError::InvalidInput {
            message: "cron scheduler not available".into(),
        })?;

        let job = CronJob::new(cron, prompt, recurring, durable);
        let id = scheduler.create(job);

        Ok(ToolOutput {
            output: id,
            is_display: true,
            images: Vec::new(),
        })
    }
}

fn get_scheduler(ctx: &ToolContext<'_>) -> Option<Arc<CronScheduler>> {
    ctx.extra
        .get(CRON_SCHEDULER_KEY)
        .and_then(|v| v.downcast_ref::<Arc<CronScheduler>>().map(Arc::clone))
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
    use crate::cron_scheduler::CronScheduler;
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        let scheduler = Arc::new(CronScheduler::new());
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(CRON_SCHEDULER_KEY.to_string(), Box::new(scheduler));
        ToolContext {
            elicit_tx: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        }
    }

    #[tokio::test]
    async fn cron_create_returns_id() {
        let output = CronCreateTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "CronCreate".into(),
                    arguments: serde_json::json!({ "cron": "* * * * *", "prompt": "check deploy" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.starts_with("cron-"));
    }

    #[tokio::test]
    async fn cron_create_missing_cron() {
        let result = CronCreateTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "CronCreate".into(),
                    arguments: serde_json::json!({ "prompt": "test" }),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
