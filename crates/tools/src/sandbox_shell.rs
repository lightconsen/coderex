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
//! an AppContainer token attached to a suspended child plus a Job Object
//! assigned after it has spawned.

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
/// (macOS profile cleanup, Windows AppContainer and Job Object). With no policy
/// configured the shell runs unwrapped, exactly as it did before.
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

    let handle = SandboxHandle::create(config.clone())
        .map_err(|error| ToolError::Io(std::io::Error::other(format!("sandbox: {error}"))))?;

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

    // rlimits on Linux; on Windows this sets `CREATE_SUSPENDED` so
    // `finish_spawn` can attach the AppContainer token; no-op on macOS. On
    // Linux with `bwrap` this installs the rlimits and deliberately skips the
    // syscall table (see `sandbox.rs`).
    handle
        .prepare(&mut std_cmd)
        .map_err(|error| ToolError::Io(std::io::Error::other(format!("sandbox: {error}"))))?;

    Ok((Command::from(std_cmd), Some(handle)))
}

/// Remove shell startup/code-injection hooks and sanitize `PATH`.
///
/// Aliases and exported functions are runtime state a static rule matcher
/// cannot see. Non-interactive `sh` does not read profiles, but inherited
/// `BASH_ENV`/`ENV`, exported functions (`BASH_FUNC_*`) and a repository-
/// controlled `PATH` entry can still change what a command means.
pub fn sanitize_shell_env(cmd: &mut Command, cwd: &std::path::Path) {
    for key in [
        "BASH_ENV",
        "ENV",
        "CDPATH",
        "GLOBIGNORE",
        "SHELLOPTS",
        "BASHOPTS",
        "PROMPT_COMMAND",
        "PS4",
        "BASH_XTRACEFD",
        "IFS",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
    ] {
        cmd.env_remove(key);
    }
    let mut exported_functions: Vec<String> = std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with("BASH_FUNC_"))
        .collect();
    exported_functions.extend(
        cmd.as_std()
            .get_envs()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .filter(|key| key.starts_with("BASH_FUNC_")),
    );
    for key in exported_functions {
        cmd.env_remove(key);
    }

    // Keep absolute system/toolchain entries. Drop relative entries and
    // entries inside the workspace, unless the workspace *is* (or contains)
    // the home directory — `PATH=~/.cargo/bin` must keep working when the
    // session runs from home.
    let home = home::home_dir();
    let safe_path: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|entry| {
                    entry.is_absolute()
                        && (!entry.starts_with(cwd)
                            || home.as_ref().is_some_and(|home| entry.starts_with(home)))
                })
                .collect()
        })
        .unwrap_or_default();
    let fallback = if cfg!(windows) {
        std::ffi::OsString::from(r"C:\Windows\System32;C:\Windows")
    } else {
        std::ffi::OsString::from("/usr/local/bin:/usr/bin:/bin")
    };
    let path = std::env::join_paths(&safe_path).unwrap_or_else(|_| fallback.clone());
    cmd.env("PATH", if safe_path.is_empty() { fallback } else { path });
}

/// Finish sandbox setup for a child that has just spawned.
///
/// On Windows the child was created suspended by `prepare`. This attaches the
/// AppContainer token, assigns the Job Object, and resumes it; any failure
/// terminates the child, so a command is never reported as sandboxed after
/// running outside the container.
///
/// On Linux the child is added to the cgroup prepared for its memory limit. A
/// cgroup attachment failure after a successful spawn is logged rather than
/// returned: the command is already running, and failing here would turn a
/// missing hardening layer into a broken shell. It is not silently treated as
/// bounded either — the warning says the child runs with `RLIMIT_AS` only.
///
/// Elsewhere there is nothing left to do. Callers must hold the handle until
/// the child exits.
///
/// # Errors
///
/// Returns an error when the container token could not be attached or the
/// child could not be assigned to its Job Object.
// The Windows implementation propagates a failure to attach the AppContainer
// token; other targets have nothing to report, so the `Result` is kept.
#[allow(clippy::unnecessary_wraps)]
// Windows and Linux do real work here; on macOS every arm is a no-op, which
// is why clippy offers `const`. One signature serves all platforms.
#[allow(clippy::missing_const_for_fn)]
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
                // The child was created suspended specifically so this path
                // could attach the container token. Losing the handle means it
                // cannot be confined, and it must not be reported as if it
                // were.
                None => tracing::warn!(
                    "the child's process handle was unavailable before the AppContainer \
                     token and Job Object could be attached; this command may have run \
                     without filesystem, network, or resource limits"
                ),
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(handle) = handle {
            if let Some(pid) = child.id() {
                if let Err(error) = handle.post_spawn_pid(pid) {
                    tracing::warn!(
                        %error,
                        "could not add the shell to its memory cgroup; it runs with \
                         RLIMIT_AS only"
                    );
                }
            } else {
                tracing::warn!(
                    "the shell exited before it could be added to its memory cgroup; \
                     it ran with RLIMIT_AS only"
                );
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

    /// The shell that exists on this host, and the arguments that make it run
    /// `script`. Windows has no extensionless `sh` on `PATH`.
    fn shell_invocation(script: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec!["/C".to_string(), script.to_string()],
            )
        } else {
            ("sh".to_string(), vec!["-c".to_string(), script.to_string()])
        }
    }

    fn args_of(args: &[String]) -> Vec<&str> {
        args.iter().map(String::as_str).collect()
    }

    /// No policy means no wrapper and no handle — the pre-sandbox behaviour.
    #[tokio::test]
    async fn without_a_policy_the_shell_runs_unwrapped() {
        let dir = std::env::temp_dir();
        let ctx = ctx_with_policy(&dir, None);
        let (shell, args) = shell_invocation("echo hi");
        let (mut cmd, handle) = sandboxed_shell(&ctx, &shell, &args_of(&args)).unwrap();

        assert!(handle.is_none());
        let output = cmd.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
    }

    /// A policy with a working directory: the wrapper (or, without one, the
    /// shell) is handed the shell as a program *or* as an argument, never both.
    ///
    /// The child must be spawned explicitly and passed through
    /// [`finish_spawn`]. On Windows `prepare` creates it `CREATE_SUSPENDED`,
    /// and a test that called `output()` directly would wait forever on a
    /// child that is never resumed — which is exactly what the Windows runtime
    /// job reported before this test was written this way.
    #[tokio::test]
    async fn a_policy_runs_the_shell_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let ctx = ctx_with_policy(
            &path,
            Some(SandboxConfig {
                allowed_read_paths: vec![path.clone()],
                allowed_write_paths: vec![path.clone()],
                additional_working_dirs: vec![path.clone()],
                ..SandboxConfig::default()
            }),
        );
        let (shell, args) = shell_invocation("echo sandboxed");
        let (mut cmd, handle) = sandboxed_shell(&ctx, &shell, &args_of(&args)).unwrap();

        assert!(
            handle.is_some(),
            "a configured policy should hand back a handle"
        );
        // `spawn` inherits stdio by default; `wait_with_output` only returns
        // what was piped, and this test asserts on the shell's output.
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let child = cmd.spawn().expect("the shell should spawn");
        finish_spawn(handle.as_ref(), &child)
            .expect("the sandbox must finish the spawn before the child runs");
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
                .await
                .expect("the shell never exited; finish_spawn may not have resumed it")
                .expect("waiting for the shell failed");

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
        assert!(guard_program_origin("./sh", dir.path()).is_err());
        assert!(guard_program_origin("/bin/sh", dir.path()).is_ok());
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::*;

    #[tokio::test]
    async fn shell_startup_hooks_and_relative_path_entries_are_removed() {
        let cwd = std::path::Path::new("/work/repo");
        let mut cmd = Command::new("sh");
        cmd.env("BASH_ENV", "/tmp/hook.sh")
            .env("ENV", "/tmp/hook.sh")
            .env("LD_PRELOAD", "/tmp/x.so")
            .env("BASH_FUNC_evil%%", "() { rm -rf /; }")
            .env("PATH", "/usr/bin:.:/work/repo/bin:/bin");
        sanitize_shell_env(&mut cmd, cwd);

        let envs: std::collections::HashMap<_, _> = cmd
            .as_std()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for key in ["BASH_ENV", "ENV", "LD_PRELOAD", "BASH_FUNC_evil%%"] {
            assert_eq!(envs.get(key), Some(&None), "{key}");
        }
        let path = envs
            .get("PATH")
            .and_then(|value| value.as_deref())
            .expect("PATH is set");
        assert!(
            !path
                .split(':')
                .any(|entry| entry == "." || entry.is_empty()),
            "{path}"
        );
        assert!(!path.contains("/work/repo"), "{path}");
        assert!(path.contains("/usr/bin"), "{path}");
    }
}
