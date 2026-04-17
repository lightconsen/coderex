//! Bash tool — shell command execution with timeout.

use std::time::Duration;

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use tokio::process::Command;

/// Default timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Maximum timeout in seconds.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Maximum output size in bytes before truncation.
const MAX_OUTPUT_BYTES: usize = 50000;

/// Execute shell commands with timeout control.
#[derive(Debug)]
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Bash".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": { "type": "string" },
                    "timeout": { "type": "number", "description": "Timeout in seconds (default 60, max 600)" },
                    "run_in_background": { "type": "boolean", "description": "Run command in background" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "exit_code": { "type": "number" },
                    "stdout": { "type": "string" },
                    "stderr": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let command = extract_string(&input.arguments, "command")?;
        let timeout_secs = extract_number(&input.arguments, "timeout")
            .unwrap_or(DEFAULT_TIMEOUT_SECS as i64)
            .clamp(1, MAX_TIMEOUT_SECS as i64) as u64;

        let result = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            Command::new("sh")
                .arg("-c")
                .arg(&command)
                .current_dir(ctx.cwd)
                .output(),
        )
        .await
        .map_err(|_| ToolError::Timeout {
            timeout: Duration::from_secs(timeout_secs),
        })?
        .map_err(ToolError::Io)?;

        let exit_code = result.status.code().unwrap_or(-1);
        let mut stdout = String::from_utf8_lossy(&result.stdout).to_string();
        let mut stderr = String::from_utf8_lossy(&result.stderr).to_string();

        let mut output = String::new();

        // Truncate stdout if too large
        if stdout.len() > MAX_OUTPUT_BYTES {
            stdout.truncate(MAX_OUTPUT_BYTES);
            stdout.push_str("\n\n... (truncated)");
        }

        // Truncate stderr if too large
        if stderr.len() > MAX_OUTPUT_BYTES {
            stderr.truncate(MAX_OUTPUT_BYTES);
            stderr.push_str("\n\n... (truncated)");
        }

        if !stdout.is_empty() {
            output.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !output.is_empty() {
                output.push_str("\n--- stderr ---\n");
            }
            output.push_str(&stderr);
        }

        output.push_str(&format!("\n\n[Exit code: {exit_code}]"));

        Ok(ToolOutput {
            output,
            is_display: true,
        })
    }
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

fn extract_number(args: &serde_json::Value, key: &str) -> Option<i64> {
    args.get(key).and_then(serde_json::Value::as_i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use crate::context::ExecutionContext;
    use std::path::Path;

    fn test_ctx(cwd: &Path) -> ToolContext<'_> {
        ToolContext::new(ExecutionContext::Normal, cwd)
    }

    fn bash_input(command: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "Bash".into(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    #[tokio::test]
    async fn simple_command() {
        let output = BashTool
            .execute(bash_input("echo hello"), test_ctx(Path::new("/tmp")))
            .await
            .unwrap();
        assert!(output.output.contains("hello"));
        assert!(output.output.contains("Exit code: 0"));
    }

    #[tokio::test]
    async fn command_with_stderr() {
        let output = BashTool
            .execute(
                bash_input("echo stderr_out >&2"),
                test_ctx(Path::new("/tmp")),
            )
            .await
            .unwrap();
        assert!(output.output.contains("stderr_out"));
    }

    #[tokio::test]
    async fn timeout_aborts() {
        let input = ToolInput {
            call_id: "2".into(),
            name: "Bash".into(),
            arguments: serde_json::json!({ "command": "sleep 10", "timeout": 1 }),
        };
        let result = BashTool.execute(input, test_ctx(Path::new("/tmp"))).await;
        assert!(matches!(result, Err(ToolError::Timeout { .. })));
    }

    #[tokio::test]
    async fn nonexistent_command_errors() {
        let output = BashTool
            .execute(
                bash_input("nonexistent_command_xyz_123"),
                test_ctx(Path::new("/tmp")),
            )
            .await
            .unwrap();
        assert!(output.output.contains("Exit code:"));
        // Just verify it failed — exact exit code varies by shell
        assert!(output.output.contains("Exit code: ") && !output.output.contains("Exit code: 0"));
    }

    #[tokio::test]
    async fn exit_code_captured() {
        let output = BashTool
            .execute(bash_input("exit 42"), test_ctx(Path::new("/tmp")))
            .await
            .unwrap();
        assert!(output.output.contains("Exit code: 42"));
    }
}
