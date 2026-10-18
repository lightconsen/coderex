//! Building a shell command under the session's sandbox policy.
//!
//! The sandbox is a property of *the shell tool*, not of one particular shell:
//! the reference wraps "the Bash tool / the PowerShell tool" with the same layer
//! and picks the shell by platform. CodeRex had the wrapping inside the Bash
//! tool only, so the PowerShell tool never read the policy the dispatcher handed
//! it — and on Windows, where PowerShell *is* the shell, nothing was confined at
//! all.
//!
//! Both shell tools now build their command here, so the platform logic exists
//! once: a Seatbelt profile on macOS, a mount namespace on Linux, and on Windows
//! a Job Object assigned after the child has spawned.

use crate::context::ToolError;
use crate::tool::ToolContext;
use coderex_core::sandbox::{SandboxConfig, SandboxHandle};
use tokio::process::Command;

/// The policy the dispatcher put in the context, if any.
#[must_use]
pub fn sandbox_config<'a>(ctx: &'a ToolContext<'_>) -> Option<&'a SandboxConfig> {
    ctx.extra
        .get("sandbox_config")
        .and_then(|value| value.downcast_ref::<SandboxConfig>())
}

/// Whether a repository-shipped program would be chosen from the working
/// directory.
///
/// A checkout can carry a binary named after a system tool (`./sh`, `./pwsh`) or
/// a `PATH` entry that points back at the workspace. Either one turns "run the
/// shell" into "run whatever the repository shipped", so a program that resolves
/// inside the working directory is refused rather than run.
pub fn guard_program_origin(program: &str, cwd: &std::path::Path) -> Result<(), ToolError> {
    if coderex_core::sandbox::resolves_inside_cwd(program, cwd) {
        return Err(ToolError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to execute '{program}': it resolves inside the working directory {}",
                cwd.display()
            ),
        )));
    }
    Ok(())
}

/// Build a shell invocation under the configured sandbox, if there is one.
///
/// Returns the command and the handle that must stay alive until it finishes
/// (macOS profile cleanup, Windows Job Object). With no policy configured the
/// shell runs unwrapped, exactly as it did before.
///
/// # Errors
///
/// Returns an error when the sandbox cannot be created or a program would come
/// out of the working directory.
pub fn sandboxed_shell(
    ctx: &ToolContext<'_>,
    shell: &str,
    shell_args: &[&str],
) -> Result<(Command, Option<SandboxHandle>), ToolError> {
    let Some(config) = sandbox_config(ctx) else {
        guard_program_origin(shell, ctx.cwd)?;
        let mut cmd = Command::new(shell);
        cmd.args(shell_args).current_dir(ctx.cwd);
        return Ok((cmd, None));
    };

    let handle = SandboxHandle::create(config.clone()).map_err(|error| {
        ToolError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("sandbox: {error}"),
        ))
    })?;

    // A wrapper, where the platform has one: `sandbox-exec` on macOS, `bwrap` on
    // Linux. The shell becomes its argument. Without a wrapper the shell *is*
    // the program — appending it as an argument too makes the fallback run
    // `sh sh -c …`, which dash reads as a script file name.
    let mut std_cmd = if let Some((wrapper, prefix_args)) = handle.sandboxed_program() {
        guard_program_origin(wrapper, ctx.cwd)?;
        let mut cmd = std::process::Command::new(wrapper);
        cmd.args(&prefix_args);
        cmd.arg(shell);
        cmd
    } else {
        std::process::Command::new(shell)
    };

    guard_program_origin(shell, ctx.cwd)?;
    std_cmd.args(shell_args).current_dir(ctx.cwd);

    // rlimits on Linux, a no-op elsewhere. On Linux with `bwrap` this installs
    // the rlimits and deliberately skips the syscall table (see `sandbox.rs`).
    handle.prepare(&mut std_cmd).map_err(|error| {
        ToolError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("sandbox: {error}"),
        ))
    })?;

    Ok((Command::from(std_cmd), Some(handle)))
}

/// Finish sandbox setup for a child that has just spawned.
///
/// On Windows the child is created first and then placed in its Job Object;
/// on Linux it is added to the cgroup prepared for its memory limit. Both are
/// only possible after the process exists. Elsewhere there is nothing left to
/// do. Callers must hold the handle until the child exits.
///
/// A cgroup attachment failure after a successful spawn is logged rather than
/// returned: the command is already running, and failing here would turn a
/// missing hardening layer into a broken shell. It is not silently treated as
/// bounded either — the warning says the child runs with `RLIMIT_AS` only.
///
/// # Errors
///
/// Returns an error when the child could not be assigned to its Job Object.
pub fn finish_spawn(
    handle: Option<&SandboxHandle>,
    child: &tokio::process::Child,
) -> Result<(), ToolError> {
    #[cfg(target_os = "windows")]
    {
        if let Some(handle) = handle {
            match child.raw_handle() {
                Some(raw) => handle.post_spawn_raw(raw).map_err(|error| {
                    ToolError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("sandbox: {error}"),
                    ))
                })?,
                // A command that finished before it could be placed in the Job
                // Object ran without its limits. Nothing can be done about it
                // after the fact — but it should not be silently treated as
                // sandboxed either.
                None => tracing::warn!(
                    "the child exited before it could be placed in its Job Object; \
                     this command ran without the sandbox's resource limits"
                ),
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(handle) = handle {
            match child.id() {
                Some(pid) => {
                    if let Err(error) = handle.post_spawn_pid(pid) {
                        tracing::warn!(
                            %error,
                            "could not add the shell to its memory cgroup; it runs with \
                             RLIMIT_AS only"
                        );
                    }
                }
                None => tracing::warn!(
                    "the shell exited before it could be added to its memory cgroup; \
                     it ran with RLIMIT_AS only"
                ),
            }
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (handle, child);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_policy(dir: &std::path::Path, config: Option<SandboxConfig>) -> ToolContext<'_> {
        let mut extra = std::collections::HashMap::new();
        if let Some(config) = config {
            extra.insert(
                "sandbox_config".to_string(),
                Box::new(config) as crate::tool::SharedState,
            );
        }
        ToolContext::new(crate::context::ExecutionContext::Normal, dir).with_extra(extra)
    }

    /// No policy means no wrapper and no handle — the pre-sandbox behaviour.
    #[tokio::test]
    async fn without_a_policy_the_shell_runs_unwrapped() {
        let dir = std::env::temp_dir();
        let ctx = ctx_with_policy(&dir, None);
        let (mut cmd, handle) = sandboxed_shell(&ctx, "sh", &["-c", "echo hi"]).unwrap();

        assert!(handle.is_none());
        let output = cmd.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
    }

    /// A policy with a working directory: the wrapper (or, without one, the
    /// shell) is handed the shell as a program *or* as an argument, never both.
    #[tokio::test]
    async fn a_policy_runs_the_shell_exactly_once() {
        let dir = std::env::temp_dir();
        let ctx = ctx_with_policy(
            &dir,
            Some(SandboxConfig {
                allowed_read_paths: vec![dir.clone()],
                allowed_write_paths: vec![dir.clone()],
                additional_working_dirs: vec![dir.clone()],
                ..SandboxConfig::default()
            }),
        );
        let (mut cmd, handle) = sandboxed_shell(&ctx, "sh", &["-c", "echo sandboxed"]).unwrap();

        assert!(
            handle.is_some(),
            "a configured policy should hand back a handle"
        );
        let output = cmd.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("sandboxed"),
            "the shell did not run (status {:?}): {stdout}",
            output.status
        );
    }

    /// A program that would come out of the working directory is refused.
    #[test]
    fn a_program_from_the_working_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let planted = dir.path().join("sh");
        std::fs::write(&planted, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // `PATH` is process-wide, so this test asks the resolver directly rather
        // than mutating it under the other tests.
        assert!(coderex_core::sandbox::resolves_inside_cwd(
            "./sh",
            dir.path()
        ));
    }
}
