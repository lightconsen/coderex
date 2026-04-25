//! CronDelete tool — cancel a scheduled cron job.

use crate::context::{ToolError, ToolOutput};
use crate::cron_scheduler::CRON_SCHEDULER_KEY;
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;

/// Cancel a cron job previously scheduled with CronCreate.
#[derive(Debug)]
pub struct CronDeleteTool;

#[async_trait]
impl Tool for CronDeleteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "CronDelete".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "deleted": { "type": "boolean" }
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
        let id = extract_string(&input.arguments, "id")?;

        let scheduler = get_scheduler(&ctx).ok_or_else(|| ToolError::InvalidInput {
            message: "cron scheduler not available".into(),
        })?;

        let deleted = scheduler.delete(&id);

        Ok(ToolOutput {
            output: if deleted { "true" } else { "false" }.to_string(),
            is_display: true,
        })
    }
}

fn get_scheduler(ctx: &ToolContext<'_>) -> Option<Arc<crate::cron_scheduler::CronScheduler>> {
    ctx.extra.get(CRON_SCHEDULER_KEY).and_then(|v| {
        v.downcast_ref::<Arc<crate::cron_scheduler::CronScheduler>>()
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
    use crate::cron_scheduler::{CronJob, CronScheduler};
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        let scheduler = Arc::new(CronScheduler::new());
        let job = CronJob::new("* * * * *".into(), "test".into(), false, false);
        scheduler.create(job);
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
    async fn cron_delete_existing() {
        let ctx = test_ctx();
        let ids = {
            let sched = ctx
                .extra
                .get(CRON_SCHEDULER_KEY)
                .and_then(|v| v.downcast_ref::<Arc<CronScheduler>>())
                .unwrap();
            sched.list().into_iter().map(|j| j.id).collect::<Vec<_>>()
        };
        let id = &ids[0];

        let output = CronDeleteTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "CronDelete".into(),
                    arguments: serde_json::json!({ "id": id }),
                },
                ctx,
            )
            .await
            .unwrap();
        assert_eq!(output.output, "true");
    }

    #[tokio::test]
    async fn cron_delete_nonexistent() {
        let output = CronDeleteTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "CronDelete".into(),
                    arguments: serde_json::json!({ "id": "nonexistent" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "false");
    }
}
