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
use coderex_core::shell_security::BashAst;
use tokio::process::Command;


/// Build the shell command under the session's sandbox policy.
///
/// The platform logic lives in [`crate::sandbox_shell`], shared with the
/// PowerShell tool: the sandbox is a property of *the shell tool*, and having it
/// inside this one is how the other shell ended up unconfined.
fn build_sandboxed_command(
    ctx: &ToolContext<'_>,
    command: &str,
) -> Result<(Command, Option<coderex_core::sandbox::SandboxHandle>), ToolError> {
    crate::sandbox_shell::sandboxed_shell(ctx, "sh", &["-c", command])
}

/// Default timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Maximum timeout in seconds.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Maximum output size in bytes before truncation.
const MAX_OUTPUT_BYTES: usize = 50000;

/// Execute shell commands with timeout control.
#[derive(Debug, Default)]
pub struct BashTool {
    /// Optional allowlist of permitted commands.
    /// When `Some`, only commands whose first token is in the list are allowed.
    allowed_commands: Option<Vec<String>>,
}

impl BashTool {
    /// Create a new Bash tool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restrict execution to a list of permitted command names.
    pub fn with_allowed_commands(mut self, commands: Vec<String>) -> Self {
        self.allowed_commands = Some(commands);
        self
    }

    /// Validate that `command` does not contain shell metacharacters
    /// and that its first token is in the allowlist (if configured).
    fn validate_command(&self, command: &str) -> Result<(), ToolError> {
        if let Some(ref allowed) = self.allowed_commands {
            // Reject common shell metacharacters that enable command injection
            let forbidden = [';', '|', '&', '`', '$', '(', ')', '<', '>'];
            if command.chars().any(|c| forbidden.contains(&c)) {
                return Err(ToolError::InvalidInput {
                    message: "shell metacharacters are not permitted when an allowlist is active"
                        .into(),
                });
            }

            let first_token = command.split_whitespace().next().unwrap_or("");
            if !allowed.iter().any(|a| a == first_token) {
                return Err(ToolError::PermissionDenied {
                    name: first_token.into(),
                });
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for BashTool {
    /// The command is this tool's rule content, as it is in the reference
    /// (`Bash=command`). Declaring the field means a deny rule is judged
    /// against the command line the call is about to run — including a command
    /// with no slash in it, which the path-shaped fallback would not offer.
    fn capabilities(&self) -> crate::tool::ToolCapabilities {
        crate::tool::ToolCapabilities {
            rule_content_field: Some(crate::tool::RuleContentField::Scalar("command")),
            ..crate::tool::ToolCapabilities::default()
        }
    }

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

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let command = extract_string(&input.arguments, "command")?;
        let timeout_secs = extract_number(&input.arguments, "timeout")
            .unwrap_or(DEFAULT_TIMEOUT_SECS as i64)
            .clamp(1, MAX_TIMEOUT_SECS as i64) as u64;

        // Validate against allowlist and metacharacters
        self.validate_command(&command)?;

        // Shell security analysis
        let ast = BashAst::parse(&command);
        let security_warning = if ast.is_dangerous() {
            Some(format!("[SECURITY WARNING: {}]\n", ast.summary()))
        } else {
            None
        };

        let (mut cmd, sandbox_handle) = build_sandboxed_command(&ctx, &command)?;

        // The shell is not a provider client, so it is given no way to read
        // this session's credentials out of the environment it inherits.
        for key in coderex_core::sandbox::credential_scrub_keys() {
            cmd.env_remove(key);
        }

        // Spawned rather than `output()`ed because Windows has to place the
        // child in its Job Object *after* it exists. The handle stays alive
        // until the child has finished, which is what keeps a macOS profile and
        // a Job Object alive for its lifetime.
        let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
            // `spawn()` inherits the parent's stdio; `output()` used to supply
            // a null stdin and piped stdout/stderr, so ask for the same here.
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
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
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
        let output = BashTool::new()
            .execute(bash_input("echo hello"), test_ctx(Path::new("/tmp")))
            .await
            .unwrap();
        assert!(output.output.contains("hello"));
        assert!(output.output.contains("Exit code: 0"));
    }

    #[tokio::test]
    async fn command_with_stderr() {
        let output = BashTool::new()
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
        let result = BashTool::new()
            .execute(input, test_ctx(Path::new("/tmp")))
            .await;
        assert!(matches!(result, Err(ToolError::Timeout { .. })));
    }

    #[tokio::test]
    async fn nonexistent_command_errors() {
        let output = BashTool::new()
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
        let output = BashTool::new()
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
        let ctx = ToolContext::new(ExecutionContext::Normal, Path::new("/tmp")).with_extra(extra);

        let output = BashTool::new()
            .execute(bash_input("echo sandboxed"), ctx)
            .await
            .unwrap();
        eprintln!("SANDBOX OUTPUT: {:?}", output.output);
        assert!(
            output.output.contains("sandboxed"),
            "output was: {}",
            output.output
        );
        assert!(
            output.output.contains("Exit code: 0"),
            "output was: {}",
            output.output
        );
    }

    /// The Linux counterpart of `sandboxed_echo_works`: the Bash tool's own
    /// spawn path, under the wrapper its policy produces.
    ///
    /// The macOS test above runs `sandbox-exec`; this one is what proves the
    /// Linux branch — `bwrap` for a mount namespace, `prepare` for rlimits —
    /// actually spawns a command. Skipped when bubblewrap is not in force,
    /// because without it the branch deliberately falls back to the syscall
    /// filter.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn sandboxed_echo_works_on_linux() {
        use std::collections::HashMap;
        use std::path::PathBuf;

        if !coderex_core::sandbox::bubblewrap_in_force() {
            eprintln!("skipping: bubblewrap is not in force");
            return;
        }

        let dir = std::env::temp_dir().join(format!("coderex-bash-sandbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir_buf = PathBuf::from(&dir);

        let mut extra = HashMap::new();
        extra.insert(
            "sandbox_config".to_string(),
            Box::new(coderex_core::sandbox::SandboxConfig {
                allowed_read_paths: vec![dir_buf.clone()],
                allowed_write_paths: vec![dir_buf.clone()],
                additional_working_dirs: vec![dir_buf],
                allow_network: false,
                ..Default::default()
            }) as crate::tool::SharedState,
        );
        let ctx = ToolContext::new(ExecutionContext::Normal, &dir).with_extra(extra);

        let output = BashTool::new()
            .execute(bash_input("echo sandboxed; echo written > inside.txt"), ctx)
            .await
            .unwrap();

        assert!(
            output.output.contains("sandboxed"),
            "output was: {}",
            output.output
        );
        assert!(
            output.output.contains("Exit code: 0"),
            "output was: {}",
            output.output
        );
        assert!(
            dir.join("inside.txt").exists(),
            "the sandboxed command could not write to the allowed path"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fallback a Linux machine without `bwrap` takes: `prepare` installs
    /// rlimits and the syscall table instead of a mount namespace.
    ///
    /// This is the path that used to take every command down with SIGSYS — the
    /// filter compared the syscall number with the architecture token, so it
    /// never matched and killed the process at its first syscall.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn sandboxed_echo_works_without_bubblewrap() {
        use std::collections::HashMap;
        use std::path::PathBuf;

        if coderex_core::sandbox::bubblewrap_in_force() {
            eprintln!("skipping: bubblewrap is in force; set CODEREX_BWRAP=0 for the syscall path");
            return;
        }

        let dir = std::env::temp_dir().join(format!("coderex-bash-seccomp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir_buf = PathBuf::from(&dir);

        let mut extra = HashMap::new();
        extra.insert(
            "sandbox_config".to_string(),
            Box::new(coderex_core::sandbox::SandboxConfig {
                allowed_read_paths: vec![dir_buf.clone()],
                allowed_write_paths: vec![dir_buf.clone()],
                additional_working_dirs: vec![dir_buf],
                allow_network: false,
                ..Default::default()
            }) as crate::tool::SharedState,
        );
        let ctx = ToolContext::new(ExecutionContext::Normal, &dir).with_extra(extra);

        let output = BashTool::new()
            .execute(bash_input("echo fallback"), ctx)
            .await
            .unwrap();

        assert!(
            output.output.contains("fallback"),
            "the syscall table broke a trivial command: {}",
            output.output
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A shell that can still read `PATH` and `HOME` but not the session's
    /// credentials. The variable is set in this process for the duration of
    /// the call; nothing else in the crate reads it.
    #[tokio::test]
    #[allow(unsafe_code)]
    async fn child_does_not_inherit_provider_credentials() {
        const KEYS: [&str; 3] = [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ];
        let previous: Vec<_> = KEYS.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for key in KEYS {
            // SAFETY: edition 2024 marks `set_var` unsafe because another
            // thread could read the environment concurrently. The only keys
            // touched are the three above, which no other test reads.
            unsafe { std::env::set_var(key, "coderex-test-secret") };
        }

        let output = BashTool::new()
            .execute(
                bash_input("printf %s \"${ANTHROPIC_AUTH_TOKEN:-STRIPPED} ${CLAUDE_CODE_OAUTH_TOKEN:-STRIPPED}\""),
                test_ctx(Path::new("/tmp")),
            )
            .await
            .unwrap();

        for (key, value) in previous {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }

        assert!(
            output.output.contains("STRIPPED STRIPPED"),
            "credentials reached the child: {}",
            output.output
        );
        assert!(!output.output.contains("coderex-test-secret"));
    }

    /// Stripping credentials must not become clearing the environment.
    #[tokio::test]
    async fn child_keeps_ordinary_environment() {
        let output = BashTool::new()
            .execute(
                bash_input("printf %s \"${HOME:+HAS_HOME}${PATH:+HAS_PATH}\""),
                test_ctx(Path::new("/tmp")),
            )
            .await
            .unwrap();
        assert!(
            output.output.contains("HAS_HOMEHAS_PATH"),
            "ordinary variables were stripped too: {}",
            output.output
        );
    }

    #[test]
    fn program_resolving_inside_cwd_is_refused() {
        let dir = std::env::temp_dir().join(format!("coderex_bash_guard_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let planted = dir.join("coderex_planted_tool");
        std::fs::write(&planted, b"#!/bin/sh\necho planted\n").unwrap();

        let error = crate::sandbox_shell::guard_program_origin("./coderex_planted_tool", &dir).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("resolves inside the working directory"),
            "unexpected error: {error}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_shell_is_not_refused() {
        // The ordinary case: `sh` comes from the system, not the workspace.
        crate::sandbox_shell::guard_program_origin("sh", Path::new("/tmp")).unwrap();
    }
}
