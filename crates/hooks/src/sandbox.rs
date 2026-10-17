//! Kernel-enforced limits for hook subprocesses.
//!
//! The limits are installed **before** the hook program runs — in `pre_exec`
//! on Unix, in a Job Object on Windows — so the hook cannot undo them. This is
//! the difference from the old shell prefix (`ulimit -t N; …; <command>`): a
//! command could end its own limit with `ulimit -v unlimited`, and the prefix
//! looked like enforcement while being advisory.
//!
//! This module covers *resource* limits only. Filesystem and network
//! confinement (bubblewrap / `sandbox-exec` / AppContainer) is a separate
//! layer and is not applied here.

use crate::engine::HookConfig;
use tokio::process::Command;

/// Apply the hook's configured limits to a command that is about to spawn.
///
/// Unix installs `RLIMIT_CPU` and `RLIMIT_FSIZE`, plus `RLIMIT_AS` on Linux.
/// macOS rejects `RLIMIT_AS`; the memory limit is therefore Linux-only rather
/// than a silent no-op that looks applied. Windows uses a Job Object through
/// [`coderex_core::sandbox`]; see [`prepare_windows_limits`].
pub(crate) fn apply_resource_limits(command: &mut Command, config: &HookConfig) {
    #[cfg(unix)]
    {
        apply_unix_limits(command, config);
    }

    #[cfg(not(unix))]
    {
        let _ = (command, config);
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn apply_unix_limits(command: &mut Command, config: &HookConfig) {
    use std::os::unix::process::CommandExt;

    let cpu_seconds = config.max_cpu_seconds;
    let file_size_bytes = config
        .max_file_size_mb
        .map(|mb| mb.saturating_mul(1024 * 1024));

    // macOS refuses RLIMIT_AS (EINVAL), so setting it there would either fail
    // the spawn or pretend to work. Memory confinement on macOS belongs to the
    // OS-sandbox layer.
    #[cfg(target_os = "linux")]
    let memory_bytes = config
        .max_memory_mb
        .map(|mb| mb.saturating_mul(1024 * 1024));
    #[cfg(not(target_os = "linux"))]
    let memory_bytes: Option<u64> = None;

    // A memory limit that cannot be installed is said out loud once, rather
    // than looking enforced because the field was accepted.
    #[cfg(not(target_os = "linux"))]
    if config.max_memory_mb.is_some() {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                "hook `max_memory_mb` is not enforced on this platform; use the OS sandbox \
                 layer for memory confinement"
            );
        });
    }

    if cpu_seconds.is_none() && file_size_bytes.is_none() && memory_bytes.is_none() {
        return;
    }

    // SAFETY: `pre_exec` runs in the forked child between fork and exec. The
    // closure captures only `Copy` values and calls `setrlimit`, which is
    // async-signal-safe. It does not allocate, log, take locks, or read any
    // parent-process state that the fork could have made inconsistent.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            if let Some(seconds) = cpu_seconds {
                let limit = libc::rlimit {
                    rlim_cur: seconds as libc::rlim_t,
                    rlim_max: seconds as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_CPU, &raw const limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            if let Some(bytes) = file_size_bytes {
                let limit = libc::rlimit {
                    rlim_cur: bytes as libc::rlim_t,
                    rlim_max: bytes as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &raw const limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            if let Some(bytes) = memory_bytes {
                let limit = libc::rlimit {
                    rlim_cur: bytes as libc::rlim_t,
                    rlim_max: bytes as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &raw const limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            Ok(())
        });
    }
}

/// Build the Job Object that limits a hook on Windows, if any limit is set.
///
/// Returns `None` when nothing is configured or the job cannot be created (a
/// warning is logged in the latter case): a missing hardening layer should not
/// make every hook unrunnable. The caller keeps the handle alive until the
/// child exits and assigns the child with [`assign_windows_limits`].
#[cfg(target_os = "windows")]
pub(crate) fn prepare_windows_limits(
    config: &HookConfig,
) -> Option<coderex_core::sandbox::SandboxHandle> {
    if config.max_cpu_seconds.is_none() && config.max_memory_mb.is_none() {
        return None;
    }

    let sandbox =
        coderex_core::sandbox::SandboxHandle::create(coderex_core::sandbox::SandboxConfig {
            allowed_read_paths: Vec::new(),
            allowed_write_paths: Vec::new(),
            allowed_exec_paths: Vec::new(),
            additional_working_dirs: Vec::new(),
            max_open_files: None,
            max_memory_bytes: config
                .max_memory_mb
                .map(|mb| mb.saturating_mul(1024 * 1024)),
            max_cpu_seconds: config.max_cpu_seconds,
            allow_network: true,
        });

    match sandbox {
        Ok(sandbox) => Some(sandbox),
        Err(error) => {
            tracing::warn!(%error, "could not create the hook's Job Object; running without limits");
            None
        }
    }
}

/// Put a spawned hook into its Job Object.
#[cfg(target_os = "windows")]
pub(crate) fn assign_windows_limits(
    sandbox: &coderex_core::sandbox::SandboxHandle,
    child: &tokio::process::Child,
) {
    let Some(handle) = child.raw_handle() else {
        tracing::warn!("the spawned hook has no process handle; Job Object limits are not applied");
        return;
    };
    if let Err(error) = sandbox.post_spawn_raw(handle) {
        tracing::warn!(%error, "could not assign the hook to its Job Object");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config() -> HookConfig {
        HookConfig {
            event: "PreToolUse".into(),
            kind: crate::engine::HookType::Command,
            command: "true".into(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5_000,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: Some(1),
            max_memory_mb: None,
            max_file_size_mb: Some(1),
            tier: crate::engine::HookTier::Core,
        }
    }

    #[tokio::test]
    async fn the_limits_are_installed_before_the_hook_runs() {
        // The hook tries to raise its own file-size limit and then write past
        // it. The write must still be cut at the configured 1 MiB; the shell's
        // exit status is not the assertion because `sh` runs the next command
        // after the one that hit the limit.
        let target =
            std::env::temp_dir().join(format!("coderex-hook-limit-sandbox-{}", std::process::id()));
        let mut hook = config();
        hook.command = format!(
            "ulimit -f unlimited 2>/dev/null || true; \
             head -c 2097152 /dev/zero > {} 2>/dev/null; \
             echo '{{\"action\":\"continue\"}}'",
            target.display()
        );
        let mut command = Command::new("sh");
        command.arg("-c").arg(&hook.command);
        apply_resource_limits(&mut command, &hook);

        command.output().await.unwrap();
        let written = std::fs::metadata(&target).map_or(0, |meta| meta.len());
        let _ = std::fs::remove_file(&target);

        assert!(
            written <= 1024 * 1024,
            "the hook raised RLIMIT_FSIZE and wrote {written} bytes"
        );
    }
}
