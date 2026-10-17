//! PowerShell tool — shell command execution for Windows platform.

use std::time::Duration;

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Default timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Maximum timeout in seconds.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Maximum output size in bytes before truncation.
const MAX_OUTPUT_BYTES: usize = 50000;

/// Execute PowerShell commands with timeout control.
#[derive(Debug)]
pub struct PowerShellTool;

#[async_trait]
impl Tool for PowerShellTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "PowerShell".into(),
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

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let command = extract_string(&input.arguments, "command")?;
        let timeout_secs = extract_number(&input.arguments, "timeout")
            .unwrap_or(DEFAULT_TIMEOUT_SECS as i64)
            .clamp(1, MAX_TIMEOUT_SECS as i64) as u64;

        // Build under the session's sandbox policy, exactly as the Bash tool
        // does. This used to spawn `pwsh` directly and never look at
        // `ctx.extra["sandbox_config"]` — so on Windows, where this *is* the
        // shell tool, a command ran with no Job Object and no confinement at
        // all, while the dispatcher had handed over a policy that nothing read.
        //
        // The helper also refuses a `pwsh` that resolves inside the working
        // directory: a checkout that ships one would otherwise choose what
        // "run PowerShell" means.
        let (mut cmd, sandbox_handle) =
            crate::sandbox_shell::sandboxed_shell(&ctx, "pwsh", &["-Command", &command])?;

        // The shell is not a provider client, so it is given no way to read
        // this session's credentials out of the environment it inherits.
        for key in coderex_core::sandbox::credential_scrub_keys() {
            cmd.env_remove(key);
        }

        // Spawned rather than `output()`ed because Windows has to place the
        // child in its Job Object after it exists.
        let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let child = cmd.spawn().map_err(ToolError::Io)?;
            crate::sandbox_shell::finish_spawn(sandbox_handle.as_ref(), &child)?;
            child.wait_with_output().await.map_err(ToolError::Io)
        })
        .await
        .map_err(|_| ToolError::Timeout {
            timeout: Duration::from_secs(timeout_secs),
        })??;

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
#[allow(dead_code)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::path::Path;

    fn test_ctx(cwd: &Path) -> ToolContext<'_> {
        ToolContext::new(ExecutionContext::Normal, cwd)
    }

    fn ps_input(command: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "PowerShell".into(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    #[tokio::test]
    async fn descriptor_is_baseline() {
        let desc = PowerShellTool.descriptor();
        assert_eq!(desc.name, "PowerShell");
        assert!(matches!(desc.tier, ToolTier::Baseline));
        assert!(!desc.is_read_only);
        assert!(desc.is_concurrency_safe);
    }

    #[test]
    fn extract_string_returns_error_for_missing_key() {
        let result = extract_string(&serde_json::json!({}), "command");
        assert!(result.is_err());
    }

    #[test]
    fn extract_number_returns_none_for_missing_key() {
        assert!(extract_number(&serde_json::json!({}), "timeout").is_none());
    }

    #[test]
    fn extract_number_parses_value() {
        assert_eq!(
            extract_number(&serde_json::json!({ "timeout": 30 }), "timeout"),
            Some(30)
        );
    }
}
