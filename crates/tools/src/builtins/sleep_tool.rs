//! Sleep tool — controlled delay.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Maximum sleep duration in seconds.
const MAX_SLEEP_SECS: u64 = 600;

/// Controlled delay for testing and sequencing.
#[derive(Debug)]
pub struct SleepTool;

#[async_trait]
impl Tool for SleepTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Sleep".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["duration"],
                "properties": {
                    "duration": { "type": "number", "description": "Seconds to sleep (max 600)" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "slept": { "type": "number" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::Interruptible,
        }
    }

    async fn execute(&self, input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let duration_secs = input
            .arguments
            .get("duration")
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| ToolError::InvalidInput {
                message: "missing required field: duration".into(),
            })?;

        if duration_secs < 0.0 {
            return Err(ToolError::InvalidInput {
                message: "duration must be non-negative".into(),
            });
        }

        let capped = duration_secs > MAX_SLEEP_SECS as f64;
        let duration_secs = duration_secs.min(MAX_SLEEP_SECS as f64);

        if !capped {
            let dur = std::time::Duration::from_millis((duration_secs * 1000.0) as u64);
            tokio::time::sleep(dur).await;
        }

        Ok(ToolOutput {
            output: format!("Slept for {duration_secs:.1}s"),
            is_display: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use crate::context::ExecutionContext;
    use std::path::Path;
    use std::time::Instant;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext::new(ExecutionContext::Normal, Path::new("/tmp"))
    }

    fn sleep_input(duration_secs: f64) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "Sleep".into(),
            arguments: serde_json::json!({ "duration": duration_secs }),
        }
    }

    #[tokio::test]
    async fn sleep_zero() {
        let start = Instant::now();
        let output = SleepTool
            .execute(sleep_input(0.0), test_ctx())
            .await
            .unwrap();
        assert!(output.output.contains("0.0s"));
        assert!(start.elapsed().as_millis() < 100);
    }

    #[tokio::test]
    async fn sleep_small_duration() {
        let start = Instant::now();
        let output = SleepTool
            .execute(sleep_input(0.05), test_ctx())
            .await
            .unwrap();
        assert!(output.output.contains("0.0s") || output.output.contains("0.1s"));
        assert!(start.elapsed().as_millis() >= 40);
    }

    #[tokio::test]
    async fn max_sleep_capped() {
        let output = SleepTool
            .execute(sleep_input(9999.0), test_ctx())
            .await
            .unwrap();
        assert!(output.output.contains("600.0s"));
    }
}
