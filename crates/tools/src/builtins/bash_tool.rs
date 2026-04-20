//! Bash tool — shell command execution with timeout.
//!
//! When a [`SandboxConfig`] is present in `ToolContext.extra` under the key
//! `"sandbox_config"`, the command is executed inside the platform sandbox:
//!
//! - **Linux**: seccomp BPF + rlimits applied via `pre_exec`.
//! - **macOS**: command rewritten to run through `sandbox-exec`.
//! - **Windows**: Job Object (requires synchronous spawn).

use std::time::Duration;

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use tokio::process::Command;
use coderex_core::shell_security::BashAst;

/// Build a `tokio::process::Command` for the given shell command,
/// applying sandbox wrapping if a [`SandboxConfig`] is present in
/// `ctx.extra`.
///
/// Returns the command plus an optional `SandboxHandle` that must be
/// kept alive until the command finishes (macOS profile cleanup).
fn build_sandboxed_command(
    ctx: &ToolContext<'_>,
    command: &str,
) -> Result<(Command, Option<coderex_core::sandbox::SandboxHandle>), ToolError> {
    let sandbox_config = ctx
        .extra
        .get("sandbox_config")
        .and_then(|v| v.downcast_ref::<coderex_core::sandbox::SandboxConfig>());

    #[cfg(target_os = "macos")]
    {
        if let Some(config) = sandbox_config {
            let handle = coderex_core::sandbox::SandboxHandle::create(config.clone())
                .map_err(|e| ToolError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("sandbox: {e}"),
                )))?;
            let (wrapper, prefix_args) = handle.sandboxed_program().ok_or_else(|| {
                ToolError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "sandbox: no wrapper program available",
                ))
            })?;
            let mut cmd = Command::new(wrapper);
            for arg in prefix_args {
                cmd.arg(arg);
            }
            cmd.arg("sh").arg("-c").arg(command);
            cmd.current_dir(ctx.cwd);
            return Ok((cmd, Some(handle)));
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(config) = sandbox_config {
            let handle = coderex_core::sandbox::SandboxHandle::create(config.clone())
                .map_err(|e| ToolError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("sandbox: {e}"),
                )))?;
            let mut std_cmd = std::process::Command::new("sh");
            std_cmd.arg("-c").arg(command);
            std_cmd.current_dir(ctx.cwd);
            handle
                .prepare(&mut std_cmd)
                .map_err(|e| ToolError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("sandbox: {e}"),
                )))?;
            return Ok((Command::from(std_cmd), Some(handle)));
        }
    }

    // No sandbox (or unsupported platform with sandbox config)
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd.current_dir(ctx.cwd);
    Ok((cmd, None))
}

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

        // Shell security analysis
        let ast = BashAst::parse(&command);
        let security_warning = if ast.is_dangerous() {
            Some(format!("[SECURITY WARNING: {}]\n", ast.summary()))
        } else {
            None
        };

        let (mut cmd, _sandbox_handle) = build_sandboxed_command(&ctx, &command)?;

        let result = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            cmd.output(),
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

        // Prepend security warning if dangerous
        if let Some(warning) = security_warning {
            output.push_str(&warning);
        }

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
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            if !output.contains("--- stderr ---") {
                output.push_str("--- stderr ---\n");
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

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn sandboxed_echo_works() {
        use std::collections::HashMap;
        use std::path::PathBuf;

        let mut extra = HashMap::new();
        extra.insert(
            "sandbox_config".to_string(),
            Box::new(coderex_core::sandbox::SandboxConfig {
                allowed_read_paths: vec![PathBuf::from("/tmp")],
                allowed_write_paths: vec![PathBuf::from("/tmp")],
                allow_network: false,
                ..Default::default()
            }) as crate::tool::SharedState,
        );
        let ctx = ToolContext::new(ExecutionContext::Normal, Path::new("/tmp"))
            .with_extra(extra);

        let output = BashTool
            .execute(bash_input("echo sandboxed"), ctx)
            .await
            .unwrap();
        eprintln!("SANDBOX OUTPUT: {:?}", output.output);
        assert!(output.output.contains("sandboxed"), "output was: {}", output.output);
        assert!(output.output.contains("Exit code: 0"), "output was: {}", output.output);
    }
}
