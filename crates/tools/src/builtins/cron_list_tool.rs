//! CronList tool — list all scheduled cron jobs.

use crate::context::{ToolError, ToolOutput};
use crate::cron_scheduler::CRON_SCHEDULER_KEY;
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;

/// List all cron jobs scheduled via CronCreate.
#[derive(Debug)]
pub struct CronListTool;

#[async_trait]
impl Tool for CronListTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "CronList".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "jobs": { "type": "array" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, _input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let scheduler = get_scheduler(&ctx).ok_or_else(|| ToolError::InvalidInput {
            message: "cron scheduler not available".into(),
        })?;

        let jobs = scheduler.list();
        let output = serde_json::to_string_pretty(&jobs).unwrap_or_else(|_| "[]".into());

        Ok(ToolOutput {
            output,
            is_display: true,
        })
    }
}

fn get_scheduler(ctx: &ToolContext<'_>) -> Option<Arc<crate::cron_scheduler::CronScheduler>> {
    ctx.extra
        .get(CRON_SCHEDULER_KEY)
        .and_then(|v| v.downcast_ref::<Arc<crate::cron_scheduler::CronScheduler>>().map(Arc::clone))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron_scheduler::{CronJob, CronScheduler};
    use crate::tool::ToolContext;
    use crate::context::ExecutionContext;
    use std::collections::HashMap;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        let scheduler = Arc::new(CronScheduler::new());
        scheduler.create(CronJob::new("* * * * *".into(), "job1".into(), true, false));
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
    async fn cron_list_returns_jobs() {
        let output = CronListTool
            .execute(
                ToolInput { call_id: "1".into(), name: "CronList".into(), arguments: serde_json::json!({}) },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("job1"));
    }

    #[tokio::test]
    async fn cron_list_empty() {
        let scheduler = Arc::new(CronScheduler::new());
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(CRON_SCHEDULER_KEY.to_string(), Box::new(scheduler));
        let ctx = ToolContext {
            elicit_tx: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        };
        let output = CronListTool
            .execute(
                ToolInput { call_id: "1".into(), name: "CronList".into(), arguments: serde_json::json!({}) },
                ctx,
            )
            .await
            .unwrap();
        assert_eq!(output.output, "[]");
    }
}
