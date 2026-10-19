//! OS-level confinement for hook subprocesses.
//!
//! Two layers are installed **before** the hook program runs:
//!
//! - **Resource limits** — in `pre_exec` on Unix, in a Job Object on Windows —
//!   so the hook cannot undo them. This is the difference from the old shell
//!   prefix (`ulimit -t N; …; <command>`): a command could end its own limit
//!   with `ulimit -v unlimited`, and the prefix looked like enforcement while
//!   being advisory.
//! - **Filesystem and network confinement** — a Seatbelt profile on macOS; on
//!   Linux a bubblewrap mount and network namespace, or the Landlock
//!   path-beneath fallback when bubblewrap is unavailable. Both reuse
//!   `coderex_core::sandbox`, the same primitives the shell tools use, rather
//!   than defining a second policy language.
//! - **cgroup v2 limits (Linux)** — when `max_memory_mb` is configured, the
//!   hook is placed in a `coderex-*` cgroup with `memory.max`/`pids.max`
//!   after spawn, in addition to `RLIMIT_AS`. If the cgroup tree is not
//!   writable or delegated, the hook runs with the rlimit and a warning.
//!
//! ## Default policy
//!
//! A command hook may read the workspace, its own working directory, the
//! user's home and config directories, the system roots, and the temp
//! directory; it may write only the workspace and the temp directory; network
//! access is denied. A per-hook [`HookSandboxConfig`] extends the path sets,
//! permits the network, or opts the hook out.
//!
//! ## Escape hatches and failure
//!
//! `CODEREX_HOOK_SANDBOX=0` (or `false`/`no`) and a hook's
//! `sandbox.enabled: false` both disable confinement explicitly; resource
//! limits still apply. When confinement **cannot** be created — Windows has no
//! path isolation, Linux has neither bubblewrap nor Landlock, macOS has no
//! `sandbox-exec` — the hook is not silently run unconfined: a gating event
//! (`PreToolUse`/`UserPromptSubmit`) fails closed, and a non-gating event runs
//! with a warning. See [`enforce`].
//!
//! ## What each platform enforces
//!
//! - **macOS** — `sandbox-exec` with `(deny default)`: reads are confined to
//!   the policy paths plus system roots (metadata everywhere is allowed), and
//!   writes and network are confined. `max_memory_mb` additionally attempts a
//!   best-effort `RLIMIT_DATA` ceiling; modern Darwin rejects it with `EINVAL`
//!   and the hook still runs (the limit is not in force).
//! - **Linux** — `bwrap` when in force: the root is read-only, policy paths
//!   are the only writable mounts, and `--unshare-net` denies the network.
//!   Otherwise Landlock path-beneath rules confine reads, writes and executes
//!   (and, from ABI 4, network bind/connect), with the seccomp table and
//!   rlimits still installed. A gating hook fails closed only when neither
//!   backend exists.
//! - **Windows** — the hook is created suspended, and [`finish_windows_spawn`]
//!   attaches the AppContainer token, assigns the Job Object and resumes it.
//!   Paths, the network and resource limits are the same policy the shell
//!   tools use; `max_file_size_mb` has no Windows equivalent and is warned
//!   about. When the AppContainer runtime is missing, `SandboxHandle::create`
//!   fails and a gating hook is denied.

use crate::engine::HookConfig;
use crate::events::HookError;
use coderex_core::sandbox::{SandboxConfig, SandboxHandle};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// The environment variable that switches hook confinement off.
pub const HOOK_SANDBOX_ENV: &str = "CODEREX_HOOK_SANDBOX";

/// The result of deciding how a command hook will be confined.
pub(crate) enum SandboxPlan {
    /// A sandbox exists and the command must run through its wrapper.
    Apply(SandboxHandle),
    /// Confinement was explicitly switched off; resource limits still apply.
    Disabled(String),
    /// Confinement was requested but this platform cannot provide it.
    Unavailable(String),
}

/// Whether an environment value is an explicit negative.
///
/// Consistent with `CODEREX_BWRAP`: anything but `0`, `false` or `no` leaves
/// the sandbox on, because a sandbox that switches itself off over an
/// unexpected value is worse than one that says what it wants.
fn sandbox_disabled_by_env(value: Option<&str>) -> bool {
    matches!(value, Some("0" | "false" | "no"))
}

/// Decide what confinement a hook gets, reading the process environment.
pub(crate) fn plan_for_hook(hook: &HookConfig) -> SandboxPlan {
    plan_with(
        hook,
        sandbox_disabled_by_env(std::env::var(HOOK_SANDBOX_ENV).ok().as_deref()),
        confinement_availability(),
    )
}

/// The injectable core of [`plan_for_hook`].
fn plan_with(
    hook: &HookConfig,
    disabled_by_env: bool,
    availability: Result<(), String>,
) -> SandboxPlan {
    if disabled_by_env {
        return SandboxPlan::Disabled(format!("disabled by {HOOK_SANDBOX_ENV}"));
    }
    if hook
        .sandbox
        .as_ref()
        .is_some_and(|sandbox| !sandbox.enabled)
    {
        return SandboxPlan::Disabled("disabled by the hook's sandbox.enabled=false".to_string());
    }
    if let Err(reason) = availability {
        return SandboxPlan::Unavailable(reason);
    }
    match SandboxHandle::create(sandbox_config(hook)) {
        Ok(handle) => SandboxPlan::Apply(handle),
        Err(error) => SandboxPlan::Unavailable(error.to_string()),
    }
}

/// Turn a plan into a handle to hold until the child exits, or an error.
///
/// A gating hook that cannot be confined is an `Err`, so
/// [`fire_hook`](crate::HookRegistry) records it as a failure and denies. A
/// non-gating hook has nowhere to deny, so the failure to confine is said out
/// loud and the hook runs with resource limits only — exactly the existing
/// failure semantics for the event.
pub(crate) fn enforce(plan: SandboxPlan, gating: bool) -> Result<Option<SandboxHandle>, HookError> {
    match plan {
        SandboxPlan::Apply(handle) => Ok(Some(handle)),
        SandboxPlan::Disabled(reason) => {
            tracing::info!(
                %reason,
                "hook OS sandbox is disabled; running with resource limits only"
            );
            Ok(None)
        }
        SandboxPlan::Unavailable(reason) => {
            if gating {
                Err(HookError::SandboxUnavailable(format!(
                    "{reason}; refusing to run a gating hook without filesystem confinement \
                     (set {HOOK_SANDBOX_ENV}=0 or sandbox.enabled=false to opt out)"
                )))
            } else {
                tracing::warn!(
                    %reason,
                    "hook OS sandbox is unavailable; running without filesystem confinement"
                );
                Ok(None)
            }
        }
    }
}

/// Whether this platform can actually apply filesystem confinement.
///
/// `coderex_core::sandbox::filesystem_isolation()` reports the platform's
/// capability, but the wrapper still has to be present: a missing
/// `sandbox-exec` or `bwrap` would otherwise turn every hook into a spawn
/// failure rather than a clear policy decision.
fn confinement_availability() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if Path::new("/usr/bin/sandbox-exec").is_file() {
            Ok(())
        } else {
            Err("sandbox-exec is not installed".to_string())
        }
    }
    #[cfg(target_os = "linux")]
    {
        if coderex_core::sandbox::bubblewrap_in_force()
            || coderex_core::sandbox::landlock_in_force()
        {
            Ok(())
        } else if coderex_core::sandbox::bubblewrap_available() {
            Err(format!(
                "bubblewrap (bwrap) is installed but switched off by {}, and the Landlock \
                 fallback is switched off by {}",
                coderex_core::sandbox::BWRAP_ENV,
                coderex_core::sandbox::LANDLOCK_ENV
            ))
        } else if coderex_core::sandbox::landlock_available() {
            Err(format!(
                "bubblewrap (bwrap) is not installed and the Landlock fallback is switched \
                 off by {}",
                coderex_core::sandbox::LANDLOCK_ENV
            ))
        } else {
            Err(
                "bubblewrap (bwrap) is not installed and the kernel does not provide Landlock"
                    .to_string(),
            )
        }
    }
    #[cfg(target_os = "windows")]
    {
        // The hook engine now finishes its own spawn (`finish_windows_spawn`),
        // so the AppContainer token attach and job assignment happen on this
        // path too. Whether the runtime exists is decided by
        // `SandboxHandle::create`, which fails closed when it does not.
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err("this platform has no filesystem isolation".to_string())
    }
}

/// Build the [`SandboxConfig`] a command hook runs under.
///
/// This is a pure policy decision, so it is tested on every platform even
/// though only macOS and Linux can execute it.
///
/// The workspace is the hook's configured `cwd`, or this process's working
/// directory. Reads additionally include the home/config directories and the
/// system roots the shell and its tools need; writes include only the
/// workspace (and, outside Windows, the temp directory — Windows supplies a
/// private one). Extra paths from
/// [`HookSandboxConfig`] extend these sets rather than replacing them, so
/// naming one extra path cannot accidentally *remove* the workspace.
#[must_use]
pub(crate) fn sandbox_config(hook: &HookConfig) -> SandboxConfig {
    let workspace = workspace_dir(hook);
    #[cfg(not(target_os = "windows"))]
    let temp = canonicalize_lenient(&std::env::temp_dir());

    let mut read = vec![workspace.clone()];
    let mut write = vec![workspace.clone()];
    // Windows gives the child a private, Low-labelled temp directory and
    // exports it as TEMP/TMP/TMPDIR. Adding the user's shared temp tree here
    // would lower *its* integrity label (letting every Low-integrity process
    // write there) and make the recursive ACL walk traverse the whole tree.
    #[cfg(not(target_os = "windows"))]
    {
        read.push(temp.clone());
        write.push(temp);
    }
    let mut exec = system_exec_paths();

    // Config and home are readable, never writable: a hook may need
    // `~/.gitconfig` or a config directory, and denying it would break
    // legitimate hooks without confining the write that matters.
    //
    // Windows is excluded: read paths are granted recursively, and walking a
    // whole user profile (tens of thousands of files) turns every hook into a
    // minutes-long ACL walk. A hook that needs a file under the profile names
    // it in `sandbox.allowed_read_paths`.
    #[cfg(not(target_os = "windows"))]
    if let Some(home) = home_dir() {
        push_unique(&mut read, home);
    }
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        push_unique(&mut read, canonicalize_lenient(&config));
    }
    // Programs on `PATH` outside the system roots — Homebrew, conda, a
    // language toolchain — are part of the user's legitimate hook environment.
    // They are made readable and executable, never writable. The hook command
    // is already arbitrary code from settings; the sandbox's job is to bound
    // what it can *change*, not to decide which interpreter exists.
    //
    // Windows is deliberately excluded: its `acl_plan` already grants the
    // system roots and `PATH` directories **best-effort**, and adding them
    // here would make the grant required. That fails on directories the user
    // cannot rewrite (`C:\Windows` is owned by TrustedInstaller), which used
    // to abort the whole hook sandbox with "Access is denied".
    #[cfg(not(target_os = "windows"))]
    for dir in path_exec_dirs() {
        // A toolchain is usually `<prefix>/bin`; the interpreter needs to read
        // its own standard library under the prefix, not just the executable.
        let is_bin_dir = dir
            .file_name()
            .is_some_and(|name| name == "bin" || name == "sbin");
        if is_bin_dir {
            if let Some(prefix) = dir.parent() {
                push_unique(&mut read, canonicalize_lenient(prefix));
            }
        }
        push_unique(&mut read, dir.clone());
        push_unique(&mut exec, dir);
    }
    // Homebrew directories are on `PATH` in a normal login shell; this covers
    // a hook spawned by a session whose `PATH` is minimal.
    for dir in ["/opt/homebrew/bin", "/opt/homebrew/sbin"] {
        let path = Path::new(dir);
        if path.is_dir() {
            push_unique(&mut read, canonicalize_lenient(path));
            push_unique(&mut exec, canonicalize_lenient(path));
        }
    }

    if let Some(config) = &hook.sandbox {
        for path in &config.allowed_read_paths {
            push_unique(&mut read, canonicalize_lenient(path));
        }
        for path in &config.allowed_write_paths {
            push_unique(&mut write, canonicalize_lenient(path));
        }
        for path in &config.allowed_exec_paths {
            push_unique(&mut exec, canonicalize_lenient(path));
        }
    }

    // On Unix the hook's own `max_*` fields are installed by
    // `apply_resource_limits`; leaving these unset avoids two layers
    // disagreeing about the same limit. Windows has no pre-exec hook, so the
    // Job Object created from this config is the only place limits can live.
    #[cfg(target_os = "windows")]
    let (max_memory_bytes, max_cpu_seconds) = (
        hook.max_memory_mb
            .map(|megabytes| megabytes.saturating_mul(1024 * 1024)),
        hook.max_cpu_seconds,
    );
    #[cfg(not(target_os = "windows"))]
    let (max_memory_bytes, max_cpu_seconds) = (None, None);

    SandboxConfig {
        allowed_read_paths: read,
        allowed_write_paths: write,
        allowed_exec_paths: exec,
        additional_working_dirs: vec![workspace],
        max_open_files: None,
        max_memory_bytes,
        max_cpu_seconds,
        allow_network: hook
            .sandbox
            .as_ref()
            .is_some_and(|sandbox| sandbox.allow_network),
    }
}

/// Build the command that runs a hook, wrapped when a sandbox is in force.
///
/// The wrapper (`sandbox-exec -f <profile>` or `bwrap …`) takes the shell as
/// its argument. stdin/stdout/stderr stay inherited, so the one-way and
/// streaming line protocols are unchanged.
pub(crate) fn sandboxed_command(command: &str, sandbox: Option<&SandboxHandle>) -> Command {
    if let Some((wrapper, prefix_args)) = sandbox.and_then(SandboxHandle::sandboxed_program) {
        let mut wrapped = Command::new(wrapper);
        wrapped.args(&prefix_args).arg("sh").arg("-c").arg(command);
        return wrapped;
    }

    // Windows has no `sh` and no wrapper program: the confinement comes from
    // the AppContainer token and the Job Object, not from a command prefix.
    // `cmd /D` skips AutoRun, `/S` makes the quoting rules predictable, and
    // `/C` runs the command.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;

        let shell = std::env::var_os("COMSPEC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(
                    std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
                )
                .join("System32")
                .join("cmd.exe")
            });
        let mut direct = Command::new(shell);
        direct.args(["/D", "/S", "/C"]);
        // `raw_arg`, not `arg`: Rust would quote the command and escape its
        // inner quotes with backslashes, which cmd does not understand — a
        // command containing `"C:\path with space\file"` then fails with
        // "The filename, directory name, or volume label syntax is incorrect".
        // cmd must receive the command text verbatim and parse it itself.
        direct.as_std_mut().raw_arg(command);
        return direct;
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut direct = Command::new("sh");
        direct.arg("-c").arg(command);
        direct
    }
}

/// The directory a hook runs in: its configured `cwd`, else this process's.
fn workspace_dir(hook: &HookConfig) -> PathBuf {
    hook.cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .map_or_else(|| PathBuf::from("."), |dir| canonicalize_lenient(&dir))
}

/// The user's home directory, if the platform names one.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| canonicalize_lenient(&home))
}

/// The absolute directories on `PATH`, resolved and de-duplicated.
///
/// A relative entry means "the current directory", which is not added: a
/// repository must not gain execute permission over its own files simply by
/// putting `.` on `PATH`.
#[cfg(not(target_os = "windows"))]
fn path_exec_dirs() -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };

    let mut dirs = Vec::new();
    for dir in std::env::split_paths(&path) {
        if !dir.is_absolute() || !dir.is_dir() {
            continue;
        }
        push_unique(&mut dirs, canonicalize_lenient(&dir));
    }
    dirs
}

/// Common locations a hook may execute programs from.
fn system_exec_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        [
            "/bin",
            "/usr/bin",
            "/usr/local/bin",
            // The Command Line Tools tree carries the system `python3` and
            // `git` on a machine without Xcode; `/Library` is already one of
            // the profile's readable system roots.
            "/Library/Developer",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        ["/bin", "/usr/bin", "/usr/local/bin"]
            .into_iter()
            .map(PathBuf::from)
            .collect()
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

/// Canonicalize when the path exists, otherwise keep it as written.
///
/// `sandbox-exec` and bubblewrap match on the resolved path, so a policy that
/// names `/var/...` where the kernel sees `/private/var/...` would silently
/// allow nothing.
fn canonicalize_lenient(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Extra per-hook sandbox policy, as it appears in settings.
///
/// Absent means the default policy. Additional paths **extend** the defaults;
/// `enabled: false` turns filesystem and network confinement off for this hook
/// (resource limits still apply).
///
/// ```json
/// {
///   "event": "PreToolUse",
///   "command": "…",
///   "sandbox": {
///     "allow_network": false,
///     "allowed_read_paths": ["/srv/policy"],
///     "allowed_write_paths": ["/tmp/out"]
///   }
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HookSandboxConfig {
    /// Whether filesystem and network confinement is applied (default true).
    ///
    /// `false` is an explicit opt-out: the hook runs with resource limits but
    /// without OS confinement. A typo in this object is a load error, not a
    /// silent fallback to the default.
    pub enabled: bool,
    /// Whether the hook may use the network (default false).
    pub allow_network: bool,
    /// Extra readable paths, added to the defaults.
    pub allowed_read_paths: Vec<PathBuf>,
    /// Extra writable paths, added to the defaults.
    pub allowed_write_paths: Vec<PathBuf>,
    /// Extra executable paths, added to the defaults.
    pub allowed_exec_paths: Vec<PathBuf>,
}

impl Default for HookSandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_network: false,
            allowed_read_paths: Vec::new(),
            allowed_write_paths: Vec::new(),
            allowed_exec_paths: Vec::new(),
        }
    }
}

impl HookSandboxConfig {
    /// Reject a configuration that cannot mean what it says.
    ///
    /// Paths are resolved against the workspace by the OS, so a relative path
    /// would silently confine nothing. It is refused at load instead.
    pub(crate) fn validate(&self) -> Result<(), HookError> {
        for (field, paths) in [
            ("allowed_read_paths", &self.allowed_read_paths),
            ("allowed_write_paths", &self.allowed_write_paths),
            ("allowed_exec_paths", &self.allowed_exec_paths),
        ] {
            for path in paths {
                if !path.is_absolute() {
                    return Err(HookError::InvalidSandboxConfig(format!(
                        "sandbox.{field} entry '{}' is not an absolute path",
                        path.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Apply the hook's configured limits to a command that is about to spawn.
///
/// Unix installs `RLIMIT_CPU` and `RLIMIT_FSIZE`. Linux adds `RLIMIT_AS`;
/// macOS attempts `RLIMIT_DATA` as a **best-effort** ceiling and ignores the
/// kernel's rejection (modern Darwin returns `EINVAL` for every useful finite
/// value, as it does for `RLIMIT_AS` and `RLIMIT_RSS`). Windows uses a Job
/// Object through [`coderex_core::sandbox`]; see [`finish_windows_spawn`].
///
/// On Linux a cgroup with `memory.max` is applied after spawn in addition to
/// these limits — see [`prepare_cgroup_limits`] and [`attach_cgroup_limits`].
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

    // Linux enforces RLIMIT_AS. macOS rejects RLIMIT_AS (EINVAL) but accepts
    // RLIMIT_DATA on kernels that honour it, so it is attempted there as a
    // best-effort data-segment ceiling; the helper reports and ignores a
    // rejection instead of failing the spawn.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let memory_bytes = config
        .max_memory_mb
        .map(|mb| mb.saturating_mul(1024 * 1024));
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let memory_bytes: Option<u64> = None;

    // A memory limit that cannot even be attempted is said out loud once,
    // rather than looking enforced because the field was accepted.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
                #[cfg(target_os = "linux")]
                {
                    let limit = libc::rlimit {
                        rlim_cur: bytes as libc::rlim_t,
                        rlim_max: bytes as libc::rlim_t,
                    };
                    if libc::setrlimit(libc::RLIMIT_AS, &raw const limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                // Best effort: modern Darwin rejects RLIMIT_DATA (EINVAL) and
                // does not enforce RLIMIT_RSS, so a rejection is not a spawn
                // failure. Where it is accepted, it bounds the data segment.
                #[cfg(target_os = "macos")]
                {
                    let _ = coderex_core::sandbox::apply_darwin_memory_limit(bytes);
                }
            }

            Ok(())
        });
    }
}

/// Create the cgroup that will carry a hook's memory limit, if one is set.
///
/// Linux only. The group is created before the spawn and the child is added
/// with [`attach_cgroup_limits`] the moment it exists. A tree that is not
/// writable or does not delegate the memory controller is reported and the
/// hook falls back to `RLIMIT_AS` — never silently counted as cgroup-limited.
#[cfg(target_os = "linux")]
pub(crate) fn prepare_cgroup_limits(
    config: &HookConfig,
) -> Option<coderex_core::sandbox::CgroupMemoryLimit> {
    let bytes = config
        .max_memory_mb
        .map(|mb| mb.saturating_mul(1024 * 1024))?;

    match coderex_core::sandbox::CgroupMemoryLimit::create(
        Some(bytes),
        Some(coderex_core::sandbox::cgroup::DEFAULT_PIDS_MAX),
    ) {
        Ok(limit) => Some(limit),
        Err(error) => {
            tracing::warn!(
                %error,
                memory_bytes = bytes,
                "hook cgroup v2 memory limit unavailable; the hook runs with RLIMIT_AS only"
            );
            None
        }
    }
}

/// Add a spawned hook to its cgroup.
///
/// The child already exists, so a failure here cannot be undone by refusing
/// the spawn: it is logged, and the hook is explicitly said to be running with
/// the rlimit rather than the cgroup limit.
#[cfg(target_os = "linux")]
pub(crate) fn attach_cgroup_limits(
    limit: &coderex_core::sandbox::CgroupMemoryLimit,
    child: &tokio::process::Child,
) {
    let Some(pid) = child.id() else {
        tracing::warn!(
            "the hook exited before it could be added to its memory cgroup; \
             this hook ran with RLIMIT_AS only"
        );
        return;
    };
    if let Err(error) = limit.attach(pid) {
        tracing::warn!(
            %error,
            cgroup = %limit.path().display(),
            "could not add the hook to its memory cgroup; it runs with RLIMIT_AS only"
        );
    }
}

/// Finish the Windows spawn: attach the AppContainer token, assign the Job
/// Object, and resume the child.
///
/// The handle is the same one `enforce` produced and `prepare` suspended the
/// child for. There is no second "limits-only" job any more: a process can
/// only belong to one job without nesting, and creating a second one made the
/// first assignment lose its limits.
///
/// Any failure terminates the child. A gating hook that cannot be confined is
/// a refusal, not a hook that runs unconfined.
#[cfg(target_os = "windows")]
pub(crate) async fn finish_windows_spawn(
    sandbox: Option<&coderex_core::sandbox::SandboxHandle>,
    child: &mut tokio::process::Child,
) -> Result<(), crate::HookError> {
    let Some(sandbox) = sandbox else {
        return Ok(());
    };
    let Some(handle) = child.raw_handle() else {
        let _ = child.start_kill();
        return Err(crate::HookError::SandboxUnavailable(
            "the spawned hook has no process handle, so the AppContainer token and Job \
             Object could not be attached"
                .to_string(),
        ));
    };
    if let Err(error) = sandbox.post_spawn_raw(handle) {
        let _ = child.start_kill();
        return Err(crate::HookError::SandboxUnavailable(format!(
            "could not attach the hook to its sandbox: {error}"
        )));
    }
    Ok(())
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
            sandbox: None,
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

    /// A memory-hogging hook is constrained by the best-effort Darwin
    /// `RLIMIT_DATA` ceiling where the kernel honours it.
    ///
    /// Modern Darwin (Monterey and later) rejects `RLIMIT_DATA` with `EINVAL`
    /// just as it rejects `RLIMIT_AS`, so when the probe says the limit cannot
    /// be installed this test says so and stops asserting enforcement rather
    /// than pretending. The hook itself must still run either way — a rejected
    /// limit is not a spawn failure.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_memory_ceiling_constrains_a_memory_hogging_hook() {
        let Some(python) = python3_on_path() else {
            eprintln!("skipping: python3 is not available to allocate memory");
            return;
        };

        // Is a finite RLIMIT_DATA accepted by this kernel at all?
        let supported = std::process::Command::new("sh")
            .arg("-c")
            .arg("ulimit -d 131072 2>/dev/null")
            .status()
            .is_ok_and(|status| status.success());

        let marker = std::env::temp_dir().join(format!(
            "coderex-hook-memory-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let _ = std::fs::remove_file(&marker);

        let mut hook = config();
        hook.max_memory_mb = Some(128);
        hook.max_cpu_seconds = Some(20);
        hook.max_file_size_mb = None;
        // 512 MiB touches more than the 128 MiB data ceiling. If the limit is
        // in force the allocation fails and the marker is never written.
        hook.command = format!(
            "{python} -I -c 'a=bytearray(536870912); a[0]=1; \
             open({:?}, \"w\").write(\"allocated\")' 2>/dev/null; \
             echo '{{\"action\":\"continue\"}}'",
            marker.display()
        );

        let mut command = Command::new("sh");
        command.arg("-c").arg(&hook.command);
        apply_resource_limits(&mut command, &hook);
        let output = command.output().await.unwrap();
        let allocated = marker.exists();
        let _ = std::fs::remove_file(&marker);

        assert!(
            String::from_utf8_lossy(&output.stdout).contains("continue"),
            "a rejected memory limit must not stop the hook: {output:?}"
        );

        if supported {
            assert!(
                !allocated,
                "the hook allocated 512 MiB despite a 128 MiB RLIMIT_DATA ceiling"
            );
        } else {
            eprintln!(
                "skipping enforcement: this Darwin kernel rejects RLIMIT_DATA with EINVAL, so \
                 the best-effort ceiling cannot be installed; the hook ran unconstrained"
            );
        }
    }

    /// The first `python3` on `PATH`, for the memory test above.
    #[cfg(target_os = "macos")]
    fn python3_on_path() -> Option<String> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
            .map(|candidate| candidate.to_string_lossy().to_string())
    }
}

// ─── Sandbox policy and failure semantics ───────────────────────────────────
//
// These run on every platform: the policy is a pure decision, and `enforce`
// describes what the engine does when a platform cannot provide confinement.
#[cfg(test)]
mod policy_tests {
    use super::*;
    use crate::engine::{HookRegistry, HookTier, HookType};

    fn hook_config() -> HookConfig {
        HookConfig {
            event: "PreToolUse".to_string(),
            kind: HookType::Command,
            command: r#"echo '{"action":"continue"}'"#.to_string(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: std::collections::HashMap::new(),
            cwd: Some(std::env::temp_dir()),
            internal: false,
            timeout_ms: 5_000,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
            sandbox: None,
            tier: HookTier::Core,
        }
    }

    #[cfg(unix)]
    fn unique_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "coderex-hook-sandbox-{name}-{}",
            std::process::id()
        ))
    }

    /// The default policy writes the workspace and temp directory, reads home
    /// and config, and denies the network.
    #[test]
    fn default_policy_confines_writes_and_denies_network() {
        let hook = hook_config();
        let workspace = canonicalize_lenient(hook.cwd.as_deref().unwrap());
        let policy = sandbox_config(&hook);

        assert!(policy.allowed_write_paths.contains(&workspace));
        assert!(policy.allowed_read_paths.contains(&workspace));
        assert_eq!(policy.additional_working_dirs, vec![workspace]);
        assert!(!policy.allow_network, "network must be denied by default");

        // Unix names temp and the system binaries explicitly. Windows gets a
        // private temp directory from the sandbox and a best-effort grant of
        // the system roots/`PATH` from the ACL plan, so its policy lists
        // neither.
        #[cfg(not(target_os = "windows"))]
        {
            assert!(
                policy
                    .allowed_write_paths
                    .contains(&canonicalize_lenient(&std::env::temp_dir()))
            );
            assert!(
                !policy.allowed_exec_paths.is_empty(),
                "a hook must be able to run system tools"
            );
        }
        #[cfg(target_os = "windows")]
        {
            assert!(
                !policy
                    .allowed_write_paths
                    .contains(&canonicalize_lenient(&std::env::temp_dir())),
                "the shared temp tree must not be lowered to Low integrity"
            );
            assert!(
                policy.allowed_exec_paths.is_empty(),
                "system and PATH grants are the ACL plan's job on Windows"
            );
        }
    }

    /// Per-hook paths extend the defaults rather than replacing them, so
    /// naming one extra path cannot silently remove the workspace.
    #[test]
    fn per_hook_paths_extend_the_defaults() {
        let mut hook = hook_config();
        hook.sandbox = Some(HookSandboxConfig {
            enabled: true,
            allow_network: true,
            allowed_read_paths: vec![PathBuf::from("/srv/policy")],
            allowed_write_paths: vec![PathBuf::from("/srv/out")],
            allowed_exec_paths: vec![PathBuf::from("/srv/bin")],
        });
        let workspace = canonicalize_lenient(hook.cwd.as_deref().unwrap());
        let policy = sandbox_config(&hook);

        assert!(policy.allow_network);
        assert!(policy.allowed_read_paths.contains(&workspace));
        assert!(
            policy
                .allowed_read_paths
                .contains(&PathBuf::from("/srv/policy"))
        );
        assert!(policy.allowed_write_paths.contains(&workspace));
        assert!(
            policy
                .allowed_write_paths
                .contains(&PathBuf::from("/srv/out"))
        );
        assert!(
            policy
                .allowed_exec_paths
                .contains(&PathBuf::from("/srv/bin"))
        );
    }

    /// A hook may opt out; the plan then applies no OS confinement.
    #[test]
    fn per_hook_opt_out_disables_the_sandbox() {
        let mut hook = hook_config();
        hook.sandbox = Some(HookSandboxConfig {
            enabled: false,
            ..HookSandboxConfig::default()
        });

        let plan = plan_with(&hook, false, Ok(()));
        assert!(
            matches!(plan, SandboxPlan::Disabled(_)),
            "sandbox.enabled=false must opt out"
        );
    }

    /// `CODEREX_HOOK_SANDBOX=0` is the global escape hatch, and only an
    /// explicit negative counts.
    #[test]
    fn env_opt_out_only_on_an_explicit_negative() {
        for off in ["0", "false", "no"] {
            assert!(sandbox_disabled_by_env(Some(off)), "{off} should opt out");
            let plan = plan_with(&hook_config(), true, Err("no wrapper".to_string()));
            assert!(
                matches!(plan, SandboxPlan::Disabled(_)),
                "{off} must not turn into a sandbox failure"
            );
        }
        for on in [None, Some("1"), Some("true"), Some("yes"), Some("")] {
            assert!(
                !sandbox_disabled_by_env(on),
                "{on:?} must leave the sandbox on"
            );
        }
    }

    /// When confinement cannot be created, a gating hook fails closed rather
    /// than running unconfined.
    #[test]
    fn unavailable_sandbox_fails_closed_on_a_gating_event() {
        let plan = plan_with(
            &hook_config(),
            false,
            Err("bubblewrap (bwrap) is not installed".to_string()),
        );
        let error = match enforce(plan, true) {
            Ok(_) => panic!("a gating hook must be refused when confinement is unavailable"),
            Err(error) => error,
        };
        assert!(
            matches!(error, HookError::SandboxUnavailable(_)),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("CODEREX_HOOK_SANDBOX"),
            "the error must name the escape hatch: {error}"
        );
    }

    /// Off a gating event there is nowhere to deny, so the hook runs with
    /// resource limits and the missing confinement is warned about.
    #[test]
    fn unavailable_sandbox_runs_off_a_gate() {
        let plan = plan_with(
            &hook_config(),
            false,
            Err("bubblewrap (bwrap) is not installed".to_string()),
        );
        assert!(enforce(plan, false).unwrap().is_none());
    }

    /// A relative path would resolve against the workspace and confine
    /// nothing; the entry is refused at load and skipped. It must not fail the
    /// whole document — the rest of the hook chain still loads.
    #[test]
    fn relative_sandbox_paths_are_refused_at_load() {
        let value = serde_json::json!({
            "hooks": [
                {
                    "event": "PreToolUse",
                    "command": "echo hi",
                    "sandbox": { "allowed_read_paths": ["relative/path"] }
                },
                { "event": "PreToolUse", "command": "kept" }
            ]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        let hooks = registry.matches_for_event(&crate::events::HookEvent::PreToolUse);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "kept");
    }

    /// An unknown key in the sandbox object is visible, not ignored: the entry
    /// is skipped while the rest of the document loads.
    #[test]
    fn unknown_sandbox_fields_are_refused_at_load() {
        let value = serde_json::json!({
            "hooks": [
                {
                    "event": "PreToolUse",
                    "command": "echo hi",
                    "sandbox": { "allow_netwrok": true }
                },
                { "event": "PreToolUse", "command": "kept" }
            ]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        let hooks = registry.matches_for_event(&crate::events::HookEvent::PreToolUse);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "kept");
    }

    /// Both shapes of the `sandbox` key load: a boolean toggle and an object.
    #[test]
    fn sandbox_accepts_a_boolean_toggle_and_an_object() {
        let value = serde_json::json!({
            "hooks": [
                { "event": "PreToolUse", "command": "echo a", "sandbox": false },
                { "event": "PreToolUse", "command": "echo b",
                  "sandbox": { "allow_network": true } }
            ]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        let hooks = registry.matches_for_event(&crate::events::HookEvent::PreToolUse);

        let disabled = hooks.iter().find(|h| h.command == "echo a").unwrap();
        assert!(!disabled.sandbox.as_ref().unwrap().enabled);
        let enabled = hooks.iter().find(|h| h.command == "echo b").unwrap();
        let sandbox = enabled.sandbox.as_ref().unwrap();
        assert!(sandbox.enabled);
        assert!(sandbox.allow_network);
    }

    // ─── Runtime: the kernel agrees with the policy ─────────────────────────

    /// Run a hook through the registry and return its result.
    async fn run_hook(hook: HookConfig) -> Result<crate::engine::HookExecutionResult, HookError> {
        let event = hook
            .event
            .parse::<crate::events::HookEvent>()
            .expect("test hook names a real event");
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let input = crate::engine::HookInput {
            event: event.as_str().to_string(),
            session_id: None,
            input: None,
            context: None,
        };
        registry
            .execute_sync(&event, &input)
            .await
            .into_iter()
            .next()
            .expect("one hook was registered")
    }

    #[cfg(target_os = "macos")]
    fn sandbox_exec_available() -> bool {
        Path::new("/usr/bin/sandbox-exec").is_file()
    }

    /// A normal hook still runs and can write inside its workspace.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_hook_sandbox_allows_a_normal_hook() {
        if !sandbox_exec_available() {
            eprintln!("skipping: sandbox-exec is not available");
            return;
        }

        let workspace = unique_dir("normal");
        std::fs::create_dir_all(&workspace).unwrap();
        let inside = workspace.join("inside.txt");

        let mut hook = hook_config();
        hook.cwd = Some(workspace.clone());
        hook.command = format!(
            "printf '%s\\n' ok > {}; echo '{{\"action\":\"continue\"}}'",
            inside.display()
        );

        let result = run_hook(hook).await.expect("a normal hook should run");
        assert!(matches!(
            result.hook_result.action,
            crate::events::HookAction::Continue
        ));
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "ok");

        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// A command hook cannot write outside the policy's writable paths.
    ///
    /// The forbidden path is under `$HOME`, which is readable (so `stat` can
    /// answer) but not writable.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_hook_sandbox_denies_a_write_outside_the_allowed_paths() {
        if !sandbox_exec_available() {
            eprintln!("skipping: sandbox-exec is not available");
            return;
        }
        let Some(home) = home_dir() else {
            eprintln!("skipping: HOME is not set");
            return;
        };

        let workspace = unique_dir("denied");
        std::fs::create_dir_all(&workspace).unwrap();
        let forbidden = home.join(format!(
            ".coderex-hook-sandbox-escape-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&forbidden);

        let mut hook = hook_config();
        hook.cwd = Some(workspace.clone());
        hook.command = format!(
            "printf '%s\\n' escaped > {} 2>/dev/null; echo '{{\"action\":\"continue\"}}'",
            forbidden.display()
        );

        let result = run_hook(hook)
            .await
            .expect("the hook should still answer after its write was denied");
        assert!(matches!(
            result.hook_result.action,
            crate::events::HookAction::Continue
        ));
        assert!(
            !forbidden.exists(),
            "the hook wrote outside the allowed paths: {}",
            forbidden.display()
        );

        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// Linux confinement is bubblewrap's mount namespace. Skipped when it is
    /// not installed or has been switched off.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_hook_sandbox_denies_a_write_outside_the_allowed_paths() {
        if !coderex_core::sandbox::bubblewrap_in_force() {
            eprintln!("skipping: bubblewrap is not in force");
            return;
        }
        let Some(home) = home_dir() else {
            eprintln!("skipping: HOME is not set");
            return;
        };

        let workspace = unique_dir("linux");
        std::fs::create_dir_all(&workspace).unwrap();
        let inside = workspace.join("inside.txt");
        let forbidden = home.join(format!(
            ".coderex-hook-sandbox-escape-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&forbidden);

        let mut hook = hook_config();
        hook.cwd = Some(workspace.clone());
        hook.command = format!(
            "printf '%s\\n' ok > {}; \
             printf '%s\\n' escaped > {} 2>/dev/null; \
             echo '{{\"action\":\"continue\"}}'",
            inside.display(),
            forbidden.display()
        );

        let result = run_hook(hook).await.expect("the hook should answer");
        assert!(matches!(
            result.hook_result.action,
            crate::events::HookAction::Continue
        ));
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "ok");
        assert!(
            !forbidden.exists(),
            "the hook wrote outside the allowed paths: {}",
            forbidden.display()
        );

        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// When bubblewrap is unavailable, Landlock is the filesystem backend and
    /// a hook is confined by path-beneath rules. Skipped when bubblewrap is in
    /// force (it takes precedence) or the kernel has no Landlock.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_hook_landlock_denies_a_write_outside_the_allowed_paths() {
        if !coderex_core::sandbox::landlock_in_force() {
            eprintln!("skipping: Landlock is not the backend in force");
            return;
        }
        let Some(home) = home_dir() else {
            eprintln!("skipping: HOME is not set");
            return;
        };

        let workspace = unique_dir("landlock");
        std::fs::create_dir_all(&workspace).unwrap();
        let inside = workspace.join("inside.txt");
        let forbidden = home.join(format!(
            ".coderex-hook-landlock-escape-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&forbidden);

        let mut hook = hook_config();
        hook.cwd = Some(workspace.clone());
        hook.command = format!(
            "printf '%s\\n' ok > {}; \
             printf '%s\\n' escaped > {} 2>/dev/null; \
             echo '{{\"action\":\"continue\"}}'",
            inside.display(),
            forbidden.display()
        );

        let result = run_hook(hook).await.expect("the hook should answer");
        assert!(matches!(
            result.hook_result.action,
            crate::events::HookAction::Continue
        ));
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "ok");
        assert!(
            !forbidden.exists(),
            "Landlock let the hook write outside its write paths: {}",
            forbidden.display()
        );

        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// Only when *neither* backend exists is there no path confinement, and a
    /// gating hook must be refused rather than run unconfined.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_gating_hook_is_denied_when_no_confinement_exists() {
        if coderex_core::sandbox::bubblewrap_in_force()
            || coderex_core::sandbox::landlock_in_force()
        {
            eprintln!("skipping: bubblewrap or Landlock is in force");
            return;
        }
        let error = run_hook(hook_config())
            .await
            .expect_err("a gating hook must not run unconfined");
        assert!(
            matches!(error, HookError::SandboxUnavailable(_)),
            "{error:?}"
        );
    }

    /// A gating hook runs under the AppContainer, and its write confinement is
    /// real: the workspace is writable, a sibling outside the policy is not.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_hook_sandbox_confines_a_write() {
        if !coderex_core::sandbox::app_container_available() {
            assert!(
                std::env::var("CODEREX_REQUIRE_APP_CONTAINER").as_deref() != Ok("1"),
                "AppContainer is unavailable on this runner but \
                 CODEREX_REQUIRE_APP_CONTAINER=1"
            );
            // The refusal is the contract when the runtime is missing.
            let error = run_hook(hook_config())
                .await
                .expect_err("a gating hook must not run unconfined");
            assert!(
                matches!(error, HookError::SandboxUnavailable(_)),
                "{error:?}"
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("workspace");
        let denied = dir.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&denied).unwrap();

        let mut config = hook_config();
        config.cwd = Some(allowed.clone());
        // The final `echo {}` is the hook's answer: a command hook that
        // prints nothing is a failure, so the write probes must still leave a
        // parseable result on stdout.
        config.command = format!(
            "echo ok> \"{}\" & echo bad> \"{}\" & echo {{}}",
            allowed.join("inside.txt").display(),
            denied.join("outside.txt").display()
        );

        let result = tokio::time::timeout(std::time::Duration::from_secs(20), run_hook(config))
            .await
            .expect("the hook never finished; was the suspended child resumed?");
        assert!(result.is_ok(), "{result:?}");
        assert!(
            allowed.join("inside.txt").exists(),
            "the hook could not write in its workspace"
        );
        assert!(
            !denied.join("outside.txt").exists(),
            "the hook wrote outside its policy"
        );
    }

    /// The hook limits live in the sandbox config on Windows, because there is
    /// no `pre_exec` to install rlimits.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_hook_limits_are_in_the_sandbox_config() {
        let mut config = hook_config();
        config.max_cpu_seconds = Some(5);
        config.max_memory_mb = Some(64);
        let sandbox = sandbox_config(&config);
        assert_eq!(sandbox.max_cpu_seconds, Some(5));
        assert_eq!(sandbox.max_memory_bytes, Some(64 * 1024 * 1024));
    }
}
