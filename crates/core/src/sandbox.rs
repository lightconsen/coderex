//! Cross-platform sandbox abstraction — seccomp, macOS sandbox-exec,
//! Windows Job Objects, and a no-op fallback.
//!
//! Each platform's sandbox applies resource limits and syscall/file-access
//! restrictions before spawning a child process.
//!
//! ## Linux
//! Uses `prlimit(2)` for resource limits (RLIMIT_NOFILE, RLIMIT_AS, RLIMIT_CPU)
//! and seccomp-bpf via `prctl(PR_SET_NO_NEW_PRIVS)` + `prctl(PR_SET_SECCOMP)`
//! with a BPF filter that allows only safe syscalls.
//!
//! ## macOS
//! Generates a `sandbox-exec(1)` profile and spawns the process through
//! `/usr/bin/sandbox-exec -f <profile>`. Reads are confined to the configured
//! read paths plus a fixed set of system roots the shell and its loader need,
//! and writes to the configured write paths.
//!
//! ## Windows
//! Creates a Win32 Job Object with memory/process limits and assigns the
//! child process to it via `AssignProcessToJobObject`.
//!
//! ## Unsupported platforms
//! Falls back to a no-op sandbox that logs warnings but does not restrict.
//!
//! ## What is enforced, per platform
//! Only macOS confines the filesystem (see [`filesystem_isolation`]). Linux
//! applies a seccomp syscall allowlist and rlimits but no path-based
//! confinement, and says so on every sandbox it creates; Windows limits
//! resources, not paths. The in-process path checks in the `Edit` and `Write`
//! tools are not kernel-enforced and do not change this.
//!
//! ## Child environment
//! Separately from any platform sandbox, children are spawned without this
//! session's provider credentials — see [`credential_scrub_keys`] — and a
//! program that would be executed out of the working directory is refused —
//! see [`resolves_inside_cwd`].

#![allow(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

// ─── Error Types ────────────────────────────────────────────────────────────

/// Errors from sandbox operations.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// The sandbox backend is not supported on this platform.
    #[error("sandbox not supported on this platform")]
    Unsupported,
    /// Failed to apply resource limits.
    #[error("failed to apply rlimit: {0}")]
    RlimitError(String),
    /// Failed to configure seccomp filters.
    #[error("failed to configure seccomp: {0}")]
    SeccompError(String),
    /// Failed to create the sandbox environment.
    #[error("failed to create sandbox: {0}")]
    CreationError(String),
}

// ─── Configuration ──────────────────────────────────────────────────────────

/// Configuration for a sandboxed process.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Paths the sandboxed process is allowed to read.
    pub allowed_read_paths: Vec<PathBuf>,
    /// Paths the sandboxed process is allowed to write.
    pub allowed_write_paths: Vec<PathBuf>,
    /// Paths the sandboxed process is allowed to execute.
    pub allowed_exec_paths: Vec<PathBuf>,
    /// Additional working directories.
    pub additional_working_dirs: Vec<PathBuf>,
    /// Maximum number of open file descriptors (rlimit nofile).
    pub max_open_files: Option<u64>,
    /// Maximum virtual memory in bytes (rlimit as).
    pub max_memory_bytes: Option<u64>,
    /// Maximum CPU time in seconds (rlimit cpu).
    pub max_cpu_seconds: Option<u64>,
    /// Network access allowed.
    pub allow_network: bool,
}

// ─── Child-process hardening ────────────────────────────────────────────────

/// Environment variables whose exact names carry this session's provider
/// credentials, and are therefore never handed to a child process.
const CREDENTIAL_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_OAUTH_TOKEN",
];

/// Name prefixes whose variables are this session's identity and telemetry
/// configuration rather than the child's own business.
///
/// `CLAUDE_` covers `CLAUDE_CODE_OAUTH_TOKEN` and the `CLAUDE_CODE_*` switches;
/// `OTEL_` covers exporter endpoints and the `Authorization` headers some of
/// them carry.
const CREDENTIAL_PREFIXES: &[&str] = &["CLAUDE_", "OTEL_"];

/// Whether a single variable name is one this process keeps to itself.
///
/// The rule is "strip when the child does not need it": a tool or MCP server
/// spawned by this session is not itself a provider client, so it is given the
/// variables it names and not the session's credentials. A child that *is* a
/// provider client — one launched with an explicitly configured environment —
/// gets back whatever its own configuration names, because an explicit
/// environment entry always beats a removal.
#[must_use]
pub fn is_credential_key(key: &str) -> bool {
    CREDENTIAL_KEYS.contains(&key) || CREDENTIAL_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// Pick out the credential-bearing names from a set of environment keys.
///
/// Split from [`credential_scrub_keys`] so the classification can be tested
/// against a fixed input instead of whatever the test process happens to be
/// running with.
#[must_use]
pub fn credential_scrub_keys_from<I, K>(keys: I) -> Vec<String>
where
    I: IntoIterator<Item = K>,
    K: AsRef<str>,
{
    keys.into_iter()
        .map(|k| k.as_ref().to_string())
        .filter(|k| is_credential_key(k))
        .collect()
}

/// The credential-bearing variables present in *this* process's environment.
///
/// Callers remove each of these from the child's inherited environment. Names
/// the child's own configuration sets are exempted by the caller, which re-adds
/// them afterwards.
#[must_use]
pub fn credential_scrub_keys() -> Vec<String> {
    credential_scrub_keys_from(std::env::vars().map(|(key, _)| key))
}

/// Resolve `program` the way the loader would, searching `PATH` in order and
/// resolving relative entries against `cwd`.
///
/// Returns the first candidate that exists, which is the one `execvp(3)` would
/// pick, or `None` when nothing in the search path matches.
#[must_use]
pub fn resolve_program(program: &str, cwd: &std::path::Path) -> Option<PathBuf> {
    program_candidates(program, cwd)
        .into_iter()
        .find(|p| p.is_file())
}

/// Whether `program` would be executed from inside the working directory.
///
/// A checkout can carry a binary that shadows a system tool (`./sh`) or a
/// `PATH` entry pointing back at the workspace, and either one turns "run the
/// shell" into "run whatever the repository shipped". A program that resolves
/// inside `cwd` is refused rather than run.
#[must_use]
pub fn resolves_inside_cwd(program: &str, cwd: &std::path::Path) -> bool {
    let cwd = canonicalize_lenient(cwd);
    // Everything is inside the filesystem root, so the check has nothing to
    // say there; refusing every program would be the wrong reading.
    if cwd.parent().is_none() {
        return false;
    }
    resolve_program(program, cwd.as_path())
        .is_some_and(|resolved| canonicalize_lenient(&resolved).starts_with(&cwd))
}

/// The candidates `execvp(3)` would try, in order.
fn program_candidates(program: &str, cwd: &std::path::Path) -> Vec<PathBuf> {
    if program.contains('/') || program.contains(std::path::MAIN_SEPARATOR) {
        let path = std::path::Path::new(program);
        return vec![if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }];
    }

    let Some(path_var) = std::env::var_os("PATH") else {
        // With no `PATH`, `execvp` falls back to a built-in default that never
        // names the working directory.
        return Vec::new();
    };

    std::env::split_paths(&path_var)
        .map(|dir| {
            let base = if dir.as_os_str().is_empty() {
                // An empty entry means the current directory.
                cwd.to_path_buf()
            } else if dir.is_absolute() {
                dir
            } else {
                cwd.join(dir)
            };
            base.join(program)
        })
        .collect()
}

fn canonicalize_lenient(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

// ─── Isolation report ───────────────────────────────────────────────────────

/// What a sandbox backend actually enforces, as opposed to what it is asked to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemIsolation {
    /// The kernel confines reads and writes to the configured paths.
    Kernel,
    /// No path-based confinement: the process may read and write anywhere its
    /// user could. Callers must not describe this as filesystem isolation.
    None,
}

// Report the filesystem confinement actually in force, per platform:
//
// - macOS: `Kernel` — `sandbox-exec` applies a Seatbelt profile that restricts
//   reads to the configured paths plus the system roots, and writes to the
//   configured paths.
// - Linux: `Kernel` when bubblewrap is installed and not switched off — the
//   command runs in a mount namespace with a read-only root and only the
//   configured paths writable. `None` otherwise, where the sandbox is a seccomp
//   syscall allowlist plus rlimits and a wrapped process can still read and
//   write anything its user could.
// - Windows: `None` — the Job Object limits memory, CPU and UI access, not file
//   paths.

/// Report the filesystem confinement in force on this platform.
///
/// On Linux this is a runtime answer, not a compile-time one: it depends on
/// whether bubblewrap is installed and not switched off.
#[must_use]
#[allow(clippy::missing_const_for_fn)] // the Linux body reads the environment
pub fn filesystem_isolation() -> FilesystemIsolation {
    #[cfg(target_os = "macos")]
    {
        FilesystemIsolation::Kernel
    }
    #[cfg(target_os = "linux")]
    {
        if bubblewrap_in_force() {
            FilesystemIsolation::Kernel
        } else {
            FilesystemIsolation::None
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        FilesystemIsolation::None
    }
}

/// Whether `bwrap` is on `PATH`.
#[must_use]
#[cfg(target_os = "linux")]
pub fn bubblewrap_available() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("bwrap").is_file()))
}

/// Whether `bwrap` is on `PATH` — always false outside Linux.
#[must_use]
#[cfg(not(target_os = "linux"))]
pub const fn bubblewrap_available() -> bool {
    false
}

/// Whether the environment switched the wrapper off.
///
/// Pure, so the rule can be read and tested without touching a process-wide
/// variable: anything but an explicit negative leaves it on.
///
/// Only the Linux implementation of [`bubblewrap_in_force`] reads it, and that
/// implementation is compiled out elsewhere — the rule is still worth having
/// one definition of, and one test.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn bubblewrap_disabled_by_env(value: Option<&str>) -> bool {
    matches!(value, Some("0" | "false" | "no"))
}

/// Whether `bwrap` is on `PATH` and has not been switched off.
#[must_use]
#[cfg(target_os = "linux")]
pub fn bubblewrap_in_force() -> bool {
    bubblewrap_available() && !bubblewrap_disabled_by_env(std::env::var(BWRAP_ENV).ok().as_deref())
}

/// Whether `bwrap` is in force — always false outside Linux.
#[must_use]
#[cfg(not(target_os = "linux"))]
pub const fn bubblewrap_in_force() -> bool {
    false
}

/// The environment variable that switches the bubblewrap wrapper off.
pub const BWRAP_ENV: &str = "CODEREX_BWRAP";

/// The `bwrap` invocation for a config, when the wrapper is in force.
///
/// Cross-platform, so the shape is compiled and tested everywhere rather than
/// only on the platform that runs it. `None` means the command is not wrapped:
/// either this is not Linux, or bubblewrap is missing or switched off.
///
/// The working directory comes from
/// [`additional_working_dirs`](SandboxConfig::additional_working_dirs), which
/// the session's policy fills with the workspace first — a rebuilt root needs
/// an explicit place to start, and without one the child starts at `/`.
#[must_use]
pub fn bubblewrap_program(config: &SandboxConfig) -> Option<(&'static str, Vec<String>)> {
    if !bubblewrap_in_force() {
        return None;
    }
    let cwd = config
        .additional_working_dirs
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("/"));

    Some(("bwrap", bubblewrap_args(config, &cwd)))
}

/// The `bwrap` arguments that confine a command to the configured paths.
///
/// Pure, so the policy can be read and tested on any platform — the wrapper is
/// Linux-only, but *which* mounts it creates is a decision worth pinning
/// everywhere.
///
/// Order is the whole trick. `bwrap` applies its arguments in sequence, so the
/// read-only root is bound first and every path that is allowed to change is
/// re-bound after it; a bind that came before `--ro-bind / /` would be undone
/// by it. `cwd` is passed explicitly rather than inherited, because the child
/// starts in a namespace whose root was just rebuilt.
///
/// The caller appends the command to run; this returns options only.
#[must_use]
pub fn bubblewrap_args(config: &SandboxConfig, cwd: &std::path::Path) -> Vec<String> {
    let mut args: Vec<String> = vec![
        // Nothing of ours should outlive us in the namespace.
        "--die-with-parent".into(),
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
    ];

    // A network namespace, which is a stronger boundary than the seccomp
    // syscall table and the one thing bubblewrap can express that it cannot.
    if !config.allow_network {
        args.push("--unshare-net".into());
    }

    // The root is read-only: every read path the config names is already inside
    // it, so this is what makes the *write* set the only thing that can change.
    args.push("--ro-bind".into());
    args.push("/".into());
    args.push("/".into());

    // Fresh /proc and /dev: the root bind made both read-only, and a shell that
    // cannot write to /dev/null or see itself in /proc is not usable.
    args.push("--proc".into());
    args.push("/proc".into());
    args.push("--dev".into());
    args.push("/dev".into());
    args.push("--tmpfs".into());
    args.push("/tmp".into());

    // The writable set, after the read-only root.
    let mut writable = config.allowed_write_paths.clone();
    writable.extend(config.additional_working_dirs.iter().cloned());
    let mut seen: Vec<PathBuf> = Vec::new();
    for path in writable {
        if seen.contains(&path) {
            continue;
        }
        seen.push(path.clone());
        let path = path.to_string_lossy().to_string();
        args.push("--bind".into());
        args.push(path.clone());
        args.push(path);
    }

    args.push("--chdir".into());
    args.push(cwd.to_string_lossy().to_string());

    args
}

// ─── Public Handle ──────────────────────────────────────────────────────────

/// Handle to an active sandbox configuration.
#[allow(missing_debug_implementations)]
pub struct SandboxHandle {
    inner: PlatformSandbox,
}

impl SandboxHandle {
    /// Create and activate a sandbox with the given config.
    ///
    /// On Linux: stores config for `pre_exec` application.
    /// On macOS: generates a sandbox-exec profile.
    /// On Windows: creates a Job Object with limits.
    /// On unsupported platforms: returns a warning but proceeds.
    pub fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
        let inner = PlatformSandbox::create(config)?;
        Ok(Self { inner })
    }

    /// Prepare a `std::process::Command` to run inside the sandbox.
    ///
    /// - **Linux**: installs a `pre_exec` closure that applies rlimits and
    ///   seccomp filters in the child process before `execve`.
    /// - **macOS**: no-op. Use [`SandboxHandle::sandboxed_program`] to get the
    ///   wrapper command.
    /// - **Windows**: no-op. Use [`SandboxHandle::post_spawn`] after spawning.
    /// - **Unsupported**: no-op.
    pub fn prepare(&self, cmd: &mut std::process::Command) -> Result<(), SandboxError> {
        self.inner.prepare(cmd)
    }

    /// Complete sandbox setup after the child process has spawned.
    ///
    /// - **Windows**: assigns the child process to the Job Object.
    /// - **Other platforms**: no-op.
    pub fn post_spawn(&self, child: &std::process::Child) -> Result<(), SandboxError> {
        self.inner.post_spawn(child)
    }

    /// Complete sandbox setup for an asynchronously spawned child.
    ///
    /// The shell tools spawn through `tokio::process`, which hands out a raw
    /// handle rather than a `std::process::Child` — so the Windows path needs
    /// this entry point to put the child in its Job Object. Without a caller
    /// there was no caller at all: the Job Object was created and never
    /// assigned to anything.
    #[cfg(target_os = "windows")]
    pub fn post_spawn_raw(
        &self,
        handle: std::os::windows::io::RawHandle,
    ) -> Result<(), SandboxError> {
        self.inner.post_spawn_raw(handle)
    }

    /// The program to run a command under, when a wrapper is in force.
    ///
    /// macOS returns `("sandbox-exec", ["-f", profile_path])`; Linux returns
    /// `("bwrap", …)` when bubblewrap is available and not switched off. Both
    /// are `None` when nothing wraps the command.
    ///
    /// This was `#[cfg(target_os = "macos")]`, which meant the Linux caller in
    /// `bash_tool` could not see it — and, because that caller is itself behind
    /// a Linux `cfg`, neither the definition nor the use was ever compiled for
    /// the platform they exist for.
    #[must_use]
    pub fn sandboxed_program(&self) -> Option<(&str, Vec<String>)> {
        self.inner.sandboxed_program()
    }
}

// ─── Platform Dispatch ──────────────────────────────────────────────────────

enum PlatformSandbox {
    #[cfg(target_os = "linux")]
    Linux(LinuxSandbox),
    #[cfg(target_os = "macos")]
    MacOs(MacOsSandbox),
    #[cfg(target_os = "windows")]
    Windows(WindowsSandbox),
    Unsupported,
}

impl PlatformSandbox {
    fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
        #[cfg(target_os = "linux")]
        return LinuxSandbox::create(config).map(Self::Linux);

        #[cfg(target_os = "macos")]
        return MacOsSandbox::create(config).map(Self::MacOs);

        #[cfg(target_os = "windows")]
        return WindowsSandbox::create(config).map(Self::Windows);

        #[allow(unreachable_code)]
        {
            tracing::warn!("Sandbox not supported on this platform; proceeding without isolation");
            Ok(Self::Unsupported)
        }
    }

    #[allow(unused_variables)] // only the Linux arm reads it
    fn prepare(&self, cmd: &mut std::process::Command) -> Result<(), SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Linux(s) => s.prepare(cmd),
            #[cfg(target_os = "macos")]
            Self::MacOs(_) => Ok(()),
            #[cfg(target_os = "windows")]
            Self::Windows(_) => Ok(()),
            Self::Unsupported => Ok(()),
        }
    }

    fn post_spawn(&self, child: &std::process::Child) -> Result<(), SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Linux(s) => s.post_spawn(child),
            #[cfg(target_os = "macos")]
            Self::MacOs(s) => s.post_spawn(child),
            #[cfg(target_os = "windows")]
            Self::Windows(s) => s.post_spawn(child),
            Self::Unsupported => Ok(()),
        }
    }

    #[cfg(target_os = "windows")]
    fn post_spawn_raw(
        &self,
        handle: std::os::windows::io::RawHandle,
    ) -> Result<(), SandboxError> {
        match self {
            Self::Windows(s) => s.post_spawn_raw(handle),
            _ => Ok(()),
        }
    }

    fn sandboxed_program(&self) -> Option<(&str, Vec<String>)> {
        match self {
            #[cfg(target_os = "macos")]
            Self::MacOs(s) => Some(s.exec_command()),
            #[cfg(target_os = "linux")]
            Self::Linux(s) => s.exec_command(),
            _ => None,
        }
    }
}

// ─── Linux: rlimit + seccomp ────────────────────────────────────────────────

/// seccomp audit token and syscall allowlist for the current target.
///
/// The audit token is the filter's first comparison and a mismatch is fatal by
/// design, so hard-coding one architecture's token makes the filter kill every
/// process on any other architecture at its first syscall — the sandbox would
/// appear to work while silently killing everything it wrapped.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod seccomp_arch {
    /// `AUDIT_ARCH_X86_64`.
    pub(super) const AUDIT_ARCH: u32 = 0xC000_003E;

    /// Syscalls a shell-command profile may use.
    pub(super) const ALLOWED: &[i64] = &[
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_open,
        libc::SYS_close,
        libc::SYS_fstat,
        libc::SYS_lseek,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_brk,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_ioctl,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_access,
        libc::SYS_pipe,
        libc::SYS_dup,
        libc::SYS_dup2,
        libc::SYS_nanosleep,
        libc::SYS_getpid,
        libc::SYS_clone,
        libc::SYS_fork,
        libc::SYS_vfork,
        libc::SYS_execve,
        libc::SYS_exit,
        libc::SYS_wait4,
        libc::SYS_kill,
        libc::SYS_uname,
        libc::SYS_fcntl,
        libc::SYS_getdents,
        libc::SYS_getcwd,
        libc::SYS_chdir,
        libc::SYS_mkdir,
        libc::SYS_rmdir,
        libc::SYS_creat,
        libc::SYS_unlink,
        libc::SYS_readlink,
        libc::SYS_chmod,
        libc::SYS_umask,
        libc::SYS_getuid,
        libc::SYS_getgid,
        libc::SYS_geteuid,
        libc::SYS_getegid,
        libc::SYS_setpgid,
        libc::SYS_getppid,
        libc::SYS_getpgrp,
        libc::SYS_arch_prctl,
        libc::SYS_futex,
        libc::SYS_getdents64,
        libc::SYS_set_tid_address,
        libc::SYS_exit_group,
        libc::SYS_openat,
        libc::SYS_mkdirat,
        libc::SYS_fchownat,
        libc::SYS_newfstatat,
        libc::SYS_unlinkat,
        libc::SYS_renameat,
        libc::SYS_readlinkat,
        libc::SYS_fchmodat,
        libc::SYS_faccessat,
        libc::SYS_set_robust_list,
        libc::SYS_get_robust_list,
        libc::SYS_prlimit64,
        libc::SYS_getrandom,
        libc::SYS_execveat,
    ];

    /// Network syscalls, allowed only when the config permits network access.
    pub(super) const NETWORK: &[i64] = &[
        libc::SYS_socket,
        libc::SYS_connect,
        libc::SYS_accept,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_shutdown,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_getsockname,
        libc::SYS_getpeername,
        libc::SYS_socketpair,
        libc::SYS_setsockopt,
        libc::SYS_getsockopt,
        libc::SYS_accept4,
    ];
}

/// seccomp audit token and syscall allowlist for aarch64 (asm-generic).
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
mod seccomp_arch {
    /// `AUDIT_ARCH_AARCH64`.
    pub(super) const AUDIT_ARCH: u32 = 0xC000_00B7;

    /// Syscalls a shell-command profile may use.
    pub(super) const ALLOWED: &[i64] = &[
        libc::SYS_getcwd,
        libc::SYS_eventfd2,
        libc::SYS_dup,
        libc::SYS_dup3,
        libc::SYS_fcntl,
        libc::SYS_ioctl,
        libc::SYS_mkdirat,
        libc::SYS_unlinkat,
        libc::SYS_renameat,
        libc::SYS_faccessat,
        libc::SYS_chdir,
        libc::SYS_fchmodat,
        libc::SYS_fchownat,
        libc::SYS_openat,
        libc::SYS_close,
        libc::SYS_pipe2,
        libc::SYS_getdents64,
        libc::SYS_lseek,
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_signalfd4,
        libc::SYS_readlinkat,
        libc::SYS_newfstatat,
        libc::SYS_fstat,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_set_tid_address,
        libc::SYS_futex,
        libc::SYS_set_robust_list,
        libc::SYS_get_robust_list,
        libc::SYS_nanosleep,
        libc::SYS_kill,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_setpgid,
        libc::SYS_uname,
        libc::SYS_umask,
        libc::SYS_getpid,
        libc::SYS_getppid,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        libc::SYS_brk,
        libc::SYS_munmap,
        libc::SYS_clone,
        libc::SYS_execve,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_wait4,
        libc::SYS_prlimit64,
        libc::SYS_getrandom,
        libc::SYS_execveat,
    ];

    /// Network syscalls, allowed only when the config permits network access.
    pub(super) const NETWORK: &[i64] = &[
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_connect,
        libc::SYS_getsockname,
        libc::SYS_getpeername,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_setsockopt,
        libc::SYS_getsockopt,
        libc::SYS_shutdown,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_accept4,
    ];
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::{SandboxConfig, SandboxError};
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// Linux sandbox using seccomp + rlimit.
    pub struct LinuxSandbox {
        config: SandboxConfig,
        /// Whether commands are wrapped in `bubblewrap`.
        ///
        /// Decided once, at creation: both the warning and whether the syscall
        /// filter may be installed depend on it.
        use_bwrap: bool,
    }

    impl LinuxSandbox {
        pub fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
            let use_bwrap = super::bubblewrap_in_force();

            // Say plainly what this sandbox is, in each of the three cases,
            // because they confine very different things.
            if use_bwrap {
                tracing::info!(
                    "sandbox: filesystem isolation is enforced by bubblewrap — the root is \
                     read-only, the configured paths are writable, and network access is {}",
                    if config.allow_network {
                        "allowed"
                    } else {
                        "denied by a network namespace"
                    }
                );
            } else if super::bubblewrap_available() {
                tracing::warn!(
                    "sandbox: filesystem isolation is NOT enforced on Linux — bubblewrap is \
                     installed but switched off by {}; seccomp syscall filtering and rlimits are \
                     the only restrictions in force",
                    super::BWRAP_ENV
                );
            } else {
                tracing::warn!(
                    "sandbox: filesystem isolation is NOT enforced on Linux — bubblewrap (bwrap) \
                     is not installed; seccomp syscall filtering and rlimits are the only \
                     restrictions in force"
                );
            }

            Ok(Self { config, use_bwrap })
        }

        pub fn prepare(&self, cmd: &mut Command) -> Result<(), SandboxError> {
            let config = self.config.clone();
            let use_bwrap = self.use_bwrap;
            let apply = move || -> Result<(), std::io::Error> {
                Self::apply_rlimits(&config)?;
                // The syscall allowlist is not installed when bubblewrap wraps
                // the command: the filter would be inherited by the wrapper
                // itself, and mounting, unsharing and pivoting a root are
                // exactly the syscalls an allowlist for a shell refuses. What
                // the filter buys when it is in force — no network syscalls —
                // bubblewrap provides with a network namespace instead.
                if !use_bwrap {
                    Self::apply_seccomp(config.allow_network)?;
                }
                Ok(())
            };
            // SAFETY: pre_exec runs in the child process before exec().
            // The closure captures only `config` (Clone) and references no
            // parent-process state that would be invalid after fork.
            unsafe { cmd.pre_exec(apply) };
            Ok(())
        }

        /// The `bwrap` invocation that confines a command, when it is in force.
        pub fn exec_command(&self) -> Option<(&'static str, Vec<String>)> {
            if !self.use_bwrap {
                return None;
            }
            super::bubblewrap_program(&self.config)
        }

        pub fn post_spawn(&self, _child: &std::process::Child) -> Result<(), SandboxError> {
            Ok(())
        }

        // ─── rlimits ─────────────────────────────────────────────────────────

        fn apply_rlimits(config: &SandboxConfig) -> Result<(), std::io::Error> {
            use libc::{prlimit, rlimit, RLIMIT_AS, RLIMIT_CPU, RLIMIT_NOFILE};

            if let Some(max_files) = config.max_open_files {
                let rl = rlimit {
                    rlim_cur: max_files,
                    rlim_max: max_files,
                };
                let ret = unsafe { prlimit(0, RLIMIT_NOFILE, &rl, std::ptr::null_mut()) };
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            if let Some(max_mem) = config.max_memory_bytes {
                let rl = rlimit {
                    rlim_cur: max_mem,
                    rlim_max: max_mem,
                };
                let ret = unsafe { prlimit(0, RLIMIT_AS, &rl, std::ptr::null_mut()) };
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            if let Some(max_cpu) = config.max_cpu_seconds {
                let rl = rlimit {
                    rlim_cur: max_cpu,
                    rlim_max: max_cpu,
                };
                let ret = unsafe { prlimit(0, RLIMIT_CPU, &rl, std::ptr::null_mut()) };
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            Ok(())
        }

        // ─── seccomp BPF ─────────────────────────────────────────────────────

        fn apply_seccomp(allow_network: bool) -> Result<(), std::io::Error> {
            // Raw constants — libc may not expose all of these.
            const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
            const PR_SET_SECCOMP: libc::c_int = 22;
            const SECCOMP_MODE_FILTER: libc::c_int = 2;
            const SECCOMP_RET_ALLOW: u32 = 0x7FFF_0000;
            const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
            const SECCOMP_RET_KILL: u32 = 0x0000_0000;

            const BPF_LD: u16 = 0x00;
            const BPF_W: u16 = 0x00;
            const BPF_ABS: u16 = 0x20;
            const BPF_JMP: u16 = 0x05;
            const BPF_JEQ: u16 = 0x10;
            const BPF_K: u16 = 0x00;
            const BPF_RET: u16 = 0x06;

            #[repr(C)]
            struct sock_filter {
                code: u16,
                jt: u8,
                jf: u8,
                k: u32,
            }

            #[repr(C)]
            struct sock_fprog {
                len: u16,
                filter: *const sock_filter,
            }

            // The arch token and syscall numbers are target-specific and live
            // in `seccomp_arch`; using another architecture's table kills every
            // wrapped process at its first syscall.
            const ALLOWED: &[i64] = super::seccomp_arch::ALLOWED;

            // The vector is filled in two places — the arch prologue before the
            // syscall list is built, and the syscall pairs after it — so it is
            // declared where the prologue starts.
            let mut filter: Vec<sock_filter> = Vec::new();

            // Load `seccomp_data.arch`, which is at offset **4** — the struct is
            // `{ nr: i32, arch: u32, ip: u64, args: [u64; 6] }`. These two
            // offsets were the other way round, so the filter compared the
            // syscall *number* against the architecture token, never matched,
            // and killed every process at its first syscall.
            filter.push(sock_filter {
                code: BPF_LD + BPF_W + BPF_ABS,
                jt: 0,
                jf: 0,
                k: 4,
            });
            // If the arch matches, skip the kill and continue; if it does not,
            // fall through to it. `jt`/`jf` are offsets from the *next*
            // instruction, so matching has to jump over the kill — with the
            // offsets swapped this killed every process on the very
            // architecture the table was built for.
            filter.push(sock_filter {
                code: BPF_JMP + BPF_JEQ + BPF_K,
                jt: 1,
                jf: 0,
                k: super::seccomp_arch::AUDIT_ARCH,
            });
            filter.push(sock_filter {
                code: BPF_RET + BPF_K,
                jt: 0,
                jf: 0,
                k: SECCOMP_RET_KILL,
            });

            // Load `seccomp_data.nr`, the syscall number, at offset 0.
            filter.push(sock_filter {
                code: BPF_LD + BPF_W + BPF_ABS,
                jt: 0,
                jf: 0,
                k: 0,
            });

            // Network syscalls are absent from the base list, and the default
            // action is to kill — so without this the sandbox would block
            // network even when the config allows it.
            let mut allowed: Vec<i64> = ALLOWED.to_vec();
            if allow_network {
                allowed.extend_from_slice(super::seccomp_arch::NETWORK);
            }

            // one JEQ + RET_ALLOW pair per allowed syscall
            for &nr in &allowed {
                filter.push(sock_filter {
                    code: BPF_JMP + BPF_JEQ + BPF_K,
                    jt: 0,
                    jf: 1,
                    k: nr as u32,
                });
                filter.push(sock_filter {
                    code: BPF_RET + BPF_K,
                    jt: 0,
                    jf: 0,
                    k: SECCOMP_RET_ALLOW,
                });
            }

            // Default deny — with `EPERM`, not `KILL`.
            //
            // A 159-syscall allowlist cannot anticipate everything a real
            // program calls: glibc picks up new syscalls (`rseq`, for one) and
            // the list goes stale. With a kill default, any syscall nobody
            // listed takes the whole process down with SIGSYS — which is
            // exactly what happened to `true` the first time this ran on a
            // Linux kernel. A sandbox should refuse the call, not murder the
            // process it was asked to confine; network syscalls are still
            // absent from the allowlist when the network is denied, so they
            // fail with the same `EPERM` either way.
            filter.push(sock_filter {
                code: BPF_RET + BPF_K,
                jt: 0,
                jf: 0,
                k: SECCOMP_RET_ERRNO | (libc::EPERM as u32 & 0xffff),
            });

            let prog = sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_ptr(),
            };

            // Disable privilege escalation
            let ret = unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
            if ret != 0 {
                return Err(std::io::Error::last_os_error());
            }

            // Install BPF filter
            let ret = unsafe {
                libc::prctl(
                    PR_SET_SECCOMP,
                    SECCOMP_MODE_FILTER as libc::c_ulong,
                    &prog as *const _ as libc::c_ulong,
                    0,
                    0,
                )
            };
            if ret != 0 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
use linux_impl::LinuxSandbox;

// ─── macOS: sandbox-exec ───────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod macos_impl {
    use super::{SandboxConfig, SandboxError};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// macOS sandbox using sandbox-exec.
    pub struct MacOsSandbox {
        config: SandboxConfig,
        profile_path: Option<PathBuf>,
    }

    impl MacOsSandbox {
        pub fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
            let mut sandbox = Self {
                config,
                profile_path: None,
            };
            let profile = sandbox.generate_profile();
            let thread_id = format!("{:?}", std::thread::current().id());
            let profile_path = std::env::temp_dir().join(format!(
                "coderex_sandbox_{}_{}.sb",
                std::process::id(),
                thread_id
            ));

            fs::write(&profile_path, &profile).map_err(|e| {
                SandboxError::CreationError(format!("failed to write sandbox profile: {e}"))
            })?;

            if let Err(e) = fs::set_permissions(&profile_path, fs::Permissions::from_mode(0o400)) {
                tracing::warn!("failed to set profile permissions: {e}");
            }

            sandbox.profile_path = Some(profile_path);
            tracing::debug!("macOS sandbox profile: {:?}", sandbox.profile_path);
            Ok(sandbox)
        }

        #[must_use]
        pub fn exec_command(&self) -> (&str, Vec<String>) {
            let profile = self
                .profile_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            ("sandbox-exec", vec!["-f".to_string(), profile])
        }

        pub fn post_spawn(&self, _child: &std::process::Child) -> Result<(), SandboxError> {
            Ok(())
        }

        /// Generate a sandbox-exec profile string.
        pub fn generate_profile(&self) -> String {
            use std::fmt::Write;
            let mut profile = String::from("(version 1)\n\n(deny default)\n\n");

            // Core IPC / system lookups
            profile.push_str("(allow mach-lookup)\n");
            profile.push_str("(allow ipc-posix-shm)\n");
            profile.push_str("(allow signal (target same-sandbox))\n");
            profile.push_str("(allow sysctl-read)\n\n");

            // Reading file *contents* is confined below. Two allowances keep
            // that confinement from being fatal to every process: the root
            // directory itself, which the loader reads before anything else,
            // and metadata everywhere, so `stat` on a path still answers
            // without disclosing what is in the file.
            profile.push_str("(allow file-read-metadata)\n");
            profile.push_str("(allow file-read* (literal \"/\"))\n");
            profile.push_str("(allow process-fork)\n");
            profile.push_str("(allow process-exec-interpreter)\n\n");

            // Allow executing common system shells and utilities
            let system_exec_paths = [
                "/bin/sh",
                "/bin/bash",
                "/usr/bin/sh",
                "/usr/bin/bash",
                "/usr/bin/env",
            ];
            for path in &system_exec_paths {
                let _ = writeln!(profile, "(allow process-exec (literal \"{path}\"))");
            }
            profile.push('\n');

            // System roots the shell, its loader and the tools it runs need in
            // order to start at all. Everything outside these and the paths
            // below is unreadable; a child cannot read a file just because it
            // can name it. `/private/var/select/sh` is where the system's `sh`
            // looks up which shell to hand off to.
            let system_read_paths = [
                "/usr",
                "/bin",
                "/sbin",
                "/System",
                "/Library",
                "/private/etc",
                "/private/var/db",
                "/private/var/select",
                "/dev",
            ];
            profile.push_str("(allow file-read*\n");
            for path in &system_read_paths {
                let _ = writeln!(profile, "  (subpath \"{path}\")");
            }
            for path in &self.config.allowed_read_paths {
                let p = path.to_string_lossy();
                let _ = writeln!(profile, "  (subpath \"{p}\")");
            }
            profile.push_str(")\n\n");

            for path in &self.config.allowed_write_paths {
                let p = path.to_string_lossy();
                let _ = writeln!(profile, "(allow file-write* (subpath \"{p}\"))");
            }

            for dir in &self.config.additional_working_dirs {
                let d = dir.to_string_lossy();
                let _ = writeln!(profile, "(allow file-read* file-write* (subpath \"{d}\"))");
            }

            // `subpath` rather than `literal`: a configured exec path is
            // normally a directory such as `/usr/bin`, and `literal` on a
            // directory matches the directory itself and nothing inside it —
            // which silently allowed no program at all. `subpath` also matches
            // the path itself, so it is correct for a single binary too.
            for path in &self.config.allowed_exec_paths {
                let p = path.to_string_lossy();
                let _ = writeln!(profile, "(allow process-exec (subpath \"{p}\"))");
            }

            if self.config.allow_network {
                profile.push_str("(allow network*)\n");
            } else {
                profile.push_str("(deny network*)\n");
            }

            profile.push_str("(allow file-read-data (literal \"/dev/zero\"))\n");
            profile.push_str("(allow file-read-data (literal \"/dev/random\"))\n");
            profile.push_str("(allow file-read-data (literal \"/dev/urandom\"))\n");
            profile.push_str("(allow file-write-data (literal \"/dev/null\"))\n");

            profile
        }
    }

    impl Drop for MacOsSandbox {
        fn drop(&mut self) {
            if let Some(ref path) = self.profile_path {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[cfg(target_os = "macos")]
use macos_impl::MacOsSandbox;

// ─── Windows: Job Objects ───────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::{SandboxConfig, SandboxError};
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_UI_EXIT_WINDOWS,
        JOB_OBJECT_UI_GLOBAL_ATOMS, JOB_OBJECT_UI_HANDLES, JOB_OBJECT_UI_NO_DESKTOP,
        JOB_OBJECT_UI_READCLIPBOARD, JOB_OBJECT_UI_WRITECLIPBOARD,
    };

    /// Windows sandbox using Job Objects.
    pub struct WindowsSandbox {
        job_handle: HANDLE,
        config: SandboxConfig,
    }

    impl WindowsSandbox {
        pub fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
            let job_name_wide: Vec<u16> = OsStr::new("CodeRexSandboxJob")
                .encode_wide()
                .chain(Some(0))
                .collect();
            let job_handle = unsafe { CreateJobObjectW(ptr::null_mut(), job_name_wide.as_ptr()) };
            if job_handle == 0 {
                return Err(SandboxError::CreationError(format!(
                    "CreateJobObject failed: {}",
                    std::io::Error::last_os_error()
                )));
            }

            let mut sandbox = Self { job_handle, config };
            sandbox.apply_limits()?;
            Ok(sandbox)
        }

        fn apply_limits(&mut self) -> Result<(), SandboxError> {
            unsafe {
                let mut basic: JOBOBJECT_BASIC_LIMIT_INFORMATION = std::mem::zeroed();
                let mut limit_flags: u32 = 0;

                if let Some(max_cpu) = self.config.max_cpu_seconds {
                    basic.PerJobUserTimeLimit.QuadPart = (max_cpu * 10_000_000) as i64;
                    limit_flags |= JOB_OBJECT_LIMIT_JOB_TIME;
                }

                // Memory is a flag on the extended structure rather than one
                // that was set above, so a memory-only configuration must
                // enter here too; otherwise it silently applies no limit.
                let has_limits = limit_flags != 0 || self.config.max_memory_bytes.is_some();
                if has_limits {
                    basic.LimitFlags = limit_flags;
                    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                    info.BasicLimitInformation = basic;

                    if let Some(max_mem) = self.config.max_memory_bytes {
                        info.JobMemoryLimit = max_mem as usize;
                        info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                    }

                    let ret = SetInformationJobObject(
                        self.job_handle,
                        JobObjectExtendedLimitInformation,
                        &info as *const _ as *const _,
                        std::mem::size_of_val(&info) as u32,
                    );
                    if ret == 0 {
                        return Err(SandboxError::RlimitError(format!(
                            "SetInformationJobObject(extended) failed: {}",
                            std::io::Error::last_os_error()
                        )));
                    }
                }

                let mut ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = std::mem::zeroed();
                ui.UIRestrictionsClass = JOB_OBJECT_UI_NO_DESKTOP
                    | JOB_OBJECT_UI_EXIT_WINDOWS
                    | JOB_OBJECT_UI_GLOBAL_ATOMS
                    | JOB_OBJECT_UI_HANDLES
                    | JOB_OBJECT_UI_READCLIPBOARD
                    | JOB_OBJECT_UI_WRITECLIPBOARD;

                let ret = SetInformationJobObject(
                    self.job_handle,
                    JobObjectBasicUIRestrictions,
                    &ui as *const _ as *const _,
                    std::mem::size_of_val(&ui) as u32,
                );
                if ret == 0 {
                    tracing::warn!(
                        "UI restrictions failed (non-critical): {}",
                        std::io::Error::last_os_error()
                    );
                }
            }

            tracing::debug!("Windows Job Object limits applied");
            Ok(())
        }

        pub fn post_spawn(&self, child: &std::process::Child) -> Result<(), SandboxError> {
            use std::os::windows::io::AsRawHandle;
            self.post_spawn_raw(child.as_raw_handle())
        }

        /// Assign a process to the Job Object, by handle.
        ///
        /// The assignment is what makes the limits apply, so a child that never
        /// gets here is not sandboxed at all — which is how this subsystem used
        /// to be: the Job Object was created, and nothing was put in it.
        pub fn post_spawn_raw(
            &self,
            process: std::os::windows::io::RawHandle,
        ) -> Result<(), SandboxError> {
            let ret = unsafe { AssignProcessToJobObject(self.job_handle, process as HANDLE) };
            if ret == 0 {
                return Err(SandboxError::CreationError(format!(
                    "AssignProcessToJobObject failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
            Ok(())
        }
    }

    impl Drop for WindowsSandbox {
        fn drop(&mut self) {
            if self.job_handle != 0 {
                unsafe { CloseHandle(self.job_handle) };
            }
        }
    }
}

#[cfg(target_os = "windows")]
use windows_impl::WindowsSandbox;

// ─── SandboxBackend Trait ──────────────────────────────────────────────────

/// Trait for sandbox backends.
pub trait SandboxBackend: Send + Sync {
    /// Get the platform name.
    fn platform_name(&self) -> &str;

    /// Check if this backend is supported on the current platform.
    fn is_supported(&self) -> bool;

    /// Apply the sandbox configuration.
    fn apply(&self, config: &SandboxConfig) -> Result<(), SandboxError>;
}

// ─── NoOp Sandbox ───────────────────────────────────────────────────────────

/// No-op sandbox for unsupported platforms or testing.
#[derive(Debug)]
pub struct NoOpSandbox;

impl SandboxBackend for NoOpSandbox {
    fn platform_name(&self) -> &'static str {
        "noop"
    }

    fn is_supported(&self) -> bool {
        true
    }

    fn apply(&self, _config: &SandboxConfig) -> Result<(), SandboxError> {
        tracing::debug!("NoOpSandbox: no restrictions applied");
        Ok(())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_config_defaults() {
        let config = SandboxConfig::default();
        assert!(config.allowed_read_paths.is_empty());
        assert!(config.allowed_write_paths.is_empty());
        assert!(config.allowed_exec_paths.is_empty());
        assert!(config.additional_working_dirs.is_empty());
        assert!(config.max_open_files.is_none());
        assert!(config.max_memory_bytes.is_none());
        assert!(config.max_cpu_seconds.is_none());
        assert!(!config.allow_network);
    }

    #[test]
    fn sandbox_config_with_paths() {
        let config = SandboxConfig {
            allowed_read_paths: vec![PathBuf::from("/tmp")],
            allowed_write_paths: vec![PathBuf::from("/tmp/output")],
            allow_network: true,
            ..SandboxConfig::default()
        };
        assert_eq!(config.allowed_read_paths.len(), 1);
        assert_eq!(config.allowed_write_paths.len(), 1);
        assert!(config.allow_network);
    }

    #[test]
    fn noop_sandbox_always_supported() {
        let sandbox = NoOpSandbox;
        assert!(sandbox.is_supported());
        assert_eq!(sandbox.platform_name(), "noop");
    }

    #[test]
    fn noop_sandbox_apply_is_ok() {
        let sandbox = NoOpSandbox;
        let config = SandboxConfig::default();
        sandbox.apply(&config).unwrap();
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_sandbox_generates_profile() {
        let config = SandboxConfig {
            allowed_read_paths: vec![PathBuf::from("/tmp/test")],
            allowed_write_paths: vec![PathBuf::from("/tmp/output")],
            allowed_exec_paths: vec![PathBuf::from("/bin/ls")],
            allow_network: false,
            ..SandboxConfig::default()
        };
        let handle = SandboxHandle::create(config).unwrap();
        let (cmd, args) = handle.sandboxed_program().unwrap();
        assert_eq!(cmd, "sandbox-exec");
        assert_eq!(args[0], "-f");
        assert!(std::path::Path::new(&args[1]).exists());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_sandbox_profile_contents() {
        let config = SandboxConfig {
            allowed_read_paths: vec![PathBuf::from("/tmp/test")],
            allowed_write_paths: vec![PathBuf::from("/tmp/output")],
            allowed_exec_paths: vec![PathBuf::from("/bin/ls")],
            allow_network: false,
            ..SandboxConfig::default()
        };
        let handle = SandboxHandle::create(config).unwrap();
        let (_, args) = handle.sandboxed_program().unwrap();
        let profile_path = &args[1];
        let profile = std::fs::read_to_string(profile_path).unwrap();

        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("/tmp/test"));
        assert!(profile.contains("/tmp/output"));
        assert!(profile.contains("/bin/ls"));
        assert!(profile.contains("(deny network*)"));
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_sandbox_profile_cleanup() {
        let config = SandboxConfig {
            allowed_read_paths: vec![PathBuf::from("/tmp/test")],
            ..SandboxConfig::default()
        };
        let handle = SandboxHandle::create(config).unwrap();
        let (_, args) = handle.sandboxed_program().unwrap();
        let profile_path = std::path::PathBuf::from(&args[1]);
        assert!(profile_path.exists());
        drop(handle);
        assert!(!profile_path.exists());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_sandbox_allows_network() {
        let config = SandboxConfig {
            allow_network: true,
            ..SandboxConfig::default()
        };
        let handle = SandboxHandle::create(config).unwrap();
        let (_, args) = handle.sandboxed_program().unwrap();
        let profile = std::fs::read_to_string(&args[1]).unwrap();
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_sandbox_prepare_noop() {
        let config = SandboxConfig::default();
        let handle = SandboxHandle::create(config).unwrap();
        let mut cmd = std::process::Command::new("echo");
        handle.prepare(&mut cmd).unwrap(); // no-op on macOS
    }

    // ─── Credential scrubbing ───────────────────────────────────────────────

    #[test]
    fn provider_credentials_are_credential_keys() {
        assert!(is_credential_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(is_credential_key("ANTHROPIC_API_KEY"));
        assert!(is_credential_key("ANTHROPIC_OAUTH_TOKEN"));
        assert!(is_credential_key("CLAUDE_CODE_OAUTH_TOKEN"));
    }

    #[test]
    fn claude_and_otel_prefixes_are_credential_keys() {
        assert!(is_credential_key("CLAUDE_CODE_USE_BEDROCK"));
        assert!(is_credential_key("OTEL_EXPORTER_OTLP_HEADERS"));
    }

    #[test]
    fn ordinary_variables_are_not_credential_keys() {
        for key in ["PATH", "HOME", "TERM", "GITHUB_TOKEN", "OPENAI_API_KEY"] {
            assert!(!is_credential_key(key), "{key} should survive scrubbing");
        }
    }

    #[test]
    fn scrub_selection_keeps_only_credential_names() {
        let selected = credential_scrub_keys_from([
            "PATH",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "OTEL_TRACES_EXPORTER",
            "HOME",
        ]);
        assert_eq!(
            selected,
            vec![
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
                "OTEL_TRACES_EXPORTER".to_string(),
            ]
        );
    }

    // ─── safeSpawn ──────────────────────────────────────────────────────────

    #[test]
    fn program_in_cwd_is_refused() {
        let dir = std::env::temp_dir().join(format!("coderex_spawn_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let planted = dir.join("coderex_shadow_prog");
        std::fs::write(&planted, b"#!/bin/sh\n").unwrap();

        // Named by path, relative to the working directory.
        assert!(resolves_inside_cwd("./coderex_shadow_prog", &dir));
        // Named absolutely, inside the working directory.
        assert!(resolves_inside_cwd(planted.to_str().unwrap(), &dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_program_is_not_refused() {
        // `sh` lives in /bin, not in the working directory.
        let dir = std::env::temp_dir();
        assert!(!resolves_inside_cwd("sh", &dir));
    }

    #[test]
    fn unresolvable_program_is_not_refused() {
        // Nothing to refuse when nothing matches; the spawn reports its own
        // failure rather than this check guessing.
        assert!(!resolves_inside_cwd(
            "coderex_no_such_program_zzz",
            &std::env::temp_dir()
        ));
    }

    #[test]
    fn path_search_prefers_the_first_match_like_execvp() {
        // A directory that shadows `sh` must not be reported when the real
        // shell is found earlier in PATH — the loader would not run it either.
        let resolved = resolve_program("sh", &std::env::temp_dir()).unwrap();
        assert!(!resolved.starts_with(std::env::temp_dir()));
    }

    // ─── macOS read confinement (runtime) ───────────────────────────────────

    /// Run `script` under a generated profile and return
    /// `(stdout, stderr, success)`.
    #[cfg(target_os = "macos")]
    fn run_sandboxed(config: SandboxConfig, script: &str) -> (String, String, bool) {
        let handle = SandboxHandle::create(config).unwrap();
        let (wrapper, args) = handle.sandboxed_program().unwrap();
        let output = std::process::Command::new(wrapper)
            .args(&args)
            .arg("sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
            output.status.success(),
        )
    }

    /// Reads are confined to the configured paths. Without the fix the profile
    /// allowed `file-read*` unconditionally, so the secret was readable.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_sandbox_denies_read_outside_allowed_path() {
        // /private/tmp is the canonical form on macOS; the profile matches on
        // the resolved path.
        let root = PathBuf::from(format!("/private/tmp/coderex_read_{}", std::process::id()));
        let allowed = root.join("allowed");
        let secret = root.join("secret");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(allowed.join("f.txt"), "ALLOWED\n").unwrap();
        std::fs::write(secret.join("f.txt"), "SECRET\n").unwrap();

        let config = SandboxConfig {
            allowed_read_paths: vec![allowed.clone()],
            allowed_write_paths: vec![allowed.clone()],
            allowed_exec_paths: vec![PathBuf::from("/bin"), PathBuf::from("/usr/bin")],
            ..SandboxConfig::default()
        };

        let (out, err, _) = run_sandboxed(
            config,
            &format!(
                "cat {}/f.txt; cat {}/f.txt",
                allowed.display(),
                secret.display()
            ),
        );

        let _ = std::fs::remove_dir_all(&root);

        assert!(out.contains("ALLOWED"), "allowed read failed: {out} {err}");
        assert!(
            !out.contains("SECRET"),
            "a file outside the allowed read paths was readable: {out}"
        );
    }

    /// `ls` is a separate binary rather than a shell builtin, so running it
    /// proves the profile's `process-exec` rules name the directory and not
    /// just the directory entry.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_sandbox_allows_executing_a_tool_from_an_allowed_path() {
        let dir = PathBuf::from(format!("/private/tmp/coderex_exec_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let config = SandboxConfig {
            allowed_read_paths: vec![dir.clone()],
            allowed_write_paths: vec![dir.clone()],
            allowed_exec_paths: vec![PathBuf::from("/bin"), PathBuf::from("/usr/bin")],
            ..SandboxConfig::default()
        };

        let (_out, err, ok) = run_sandboxed(config, &format!("ls {}", dir.display()));

        let _ = std::fs::remove_dir_all(&dir);

        assert!(ok, "ls should run inside the sandbox: {err}");
    }

    // ─── Isolation report ───────────────────────────────────────────────────

    #[test]
    fn isolation_report_matches_the_platform() {
        #[cfg(target_os = "macos")]
        assert_eq!(filesystem_isolation(), FilesystemIsolation::Kernel);
        // On Linux the answer depends on bubblewrap, so the report is pinned to
        // what is actually in force rather than to the platform.
        #[cfg(target_os = "linux")]
        assert_eq!(
            filesystem_isolation(),
            if bubblewrap_in_force() {
                FilesystemIsolation::Kernel
            } else {
                FilesystemIsolation::None
            }
        );
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        assert_eq!(filesystem_isolation(), FilesystemIsolation::None);
    }

    // ─── bubblewrap policy ──────────────────────────────────────────────────
    //
    // The wrapper itself is Linux-only, but the arguments it is given are a
    // policy decision, and these run everywhere.

    fn bubblewrap_config() -> SandboxConfig {
        SandboxConfig {
            allowed_read_paths: vec![PathBuf::from("/work/project")],
            allowed_write_paths: vec![PathBuf::from("/work/project")],
            allowed_exec_paths: vec![PathBuf::from("/usr/bin")],
            additional_working_dirs: vec![PathBuf::from("/work/project")],
            max_open_files: None,
            max_memory_bytes: None,
            max_cpu_seconds: None,
            allow_network: false,
        }
    }

    /// The read-only root has to come first: `bwrap` applies its arguments in
    /// order, so a writable bind emitted before it would be undone.
    #[test]
    fn bubblewrap_binds_the_root_read_only_before_anything_writable() {
        let args = bubblewrap_args(&bubblewrap_config(), std::path::Path::new("/work/project"));

        let root = args
            .windows(3)
            .position(|w| w == ["--ro-bind", "/", "/"])
            .expect("the root should be bound read-only");
        let writable = args
            .windows(3)
            .position(|w| w[0] == "--bind" && w[1] == "/work/project")
            .expect("the workspace should be writable");

        assert!(
            root < writable,
            "the writable bind would be undone: {args:?}"
        );
    }

    #[test]
    fn bubblewrap_makes_each_writable_path_writable_once() {
        let mut config = bubblewrap_config();
        // The policy lists the workspace in both fields; a second identical
        // bind is noise, not a second permission.
        config
            .additional_working_dirs
            .push(std::path::PathBuf::from("/work/build"));

        let args = bubblewrap_args(&config, std::path::Path::new("/work/project"));

        let binds: Vec<&String> = args
            .windows(3)
            .filter(|w| w[0] == "--bind")
            .map(|w| &w[1])
            .collect();
        assert_eq!(binds, vec!["/work/project", "/work/build"]);
    }

    /// Network is a namespace question, not a syscall question, here.
    #[test]
    fn bubblewrap_denies_network_by_namespace_only_when_asked() {
        let denied = bubblewrap_args(&bubblewrap_config(), std::path::Path::new("/"));
        assert!(denied.iter().any(|a| a == "--unshare-net"));

        let mut allowed_config = bubblewrap_config();
        allowed_config.allow_network = true;
        let allowed = bubblewrap_args(&allowed_config, std::path::Path::new("/"));
        assert!(
            !allowed.iter().any(|a| a == "--unshare-net"),
            "a command that may use the network was put in a network namespace"
        );
    }

    /// Nothing is wrapped unless the wrapper is actually available: off Linux,
    /// and on a Linux host without `bwrap`, the caller falls back to the
    /// syscall filter rather than spawning a program that is not there.
    #[test]
    fn no_wrapper_is_offered_when_bubblewrap_is_not_in_force() {
        if bubblewrap_in_force() {
            // A Linux host with bwrap in force; the argument tests above cover
            // what it gets.
            return;
        }
        assert!(bubblewrap_program(&bubblewrap_config()).is_none());
    }

    #[test]
    fn bubblewrap_starts_in_the_working_directory() {
        let args = bubblewrap_args(&bubblewrap_config(), std::path::Path::new("/work/project"));

        let chdir = args
            .iter()
            .position(|a| a == "--chdir")
            .expect("a rebuilt root needs an explicit working directory");
        assert_eq!(args[chdir + 1], "/work/project");
    }

    /// The child needs a writable `/dev` and a `/proc` of its own; the
    /// read-only root bind gives it neither.
    #[test]
    fn bubblewrap_provides_fresh_dev_and_proc() {
        let args = bubblewrap_args(&bubblewrap_config(), std::path::Path::new("/"));

        for flag in ["--proc", "--dev", "--tmpfs", "--die-with-parent"] {
            assert!(args.iter().any(|a| a == flag), "{flag} missing: {args:?}");
        }
    }

    /// Anything but an explicit negative leaves the wrapper on: a sandbox that
    /// switches itself off because a variable was set to something unexpected
    /// is worse than one that says what it wants.
    /// Run `bwrap` for real with the arguments [`bubblewrap_args`] produces.
    ///
    /// Every other bubblewrap test reads the argument vector; this one executes
    /// it, so a wrong flag fails a test instead of breaking every shell command
    /// on a Linux machine. Linux-only, and skipped when `bwrap` is not
    /// installed — but it is the test that turns "the policy looks right" into
    /// "the kernel agrees".
    #[test]
    #[cfg(target_os = "linux")]
    fn the_bubblewrap_arguments_confine_a_real_process() {
        if !bubblewrap_available() {
            eprintln!("skipping: bwrap is not installed");
            return;
        }

        let dir = std::env::temp_dir().join(format!("coderex-bwrap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let config = SandboxConfig {
            allowed_read_paths: vec![dir.clone()],
            allowed_write_paths: vec![dir.clone()],
            allowed_exec_paths: Vec::new(),
            additional_working_dirs: vec![dir.clone()],
            max_open_files: None,
            max_memory_bytes: None,
            max_cpu_seconds: None,
            allow_network: false,
        };
        let args = bubblewrap_args(&config, &dir);
        let run = |script: &str| {
            std::process::Command::new("bwrap")
                .args(&args)
                .arg("sh")
                .arg("-c")
                .arg(script)
                .output()
                .expect("bwrap should be runnable")
        };

        // It starts at all, in the working directory the policy names, and the
        // writable path is writable — the three things a wrong flag breaks first.
        let out = run("pwd; echo written > inside.txt");
        assert!(
            out.status.success(),
            "bwrap refused the arguments: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            dir.to_string_lossy(),
            "the child did not start in the configured working directory"
        );
        assert!(
            dir.join("inside.txt").exists(),
            "the allowed write path was not writable"
        );

        // The root is read-only.
        let out = run("touch /coderex-should-not-exist 2>&1 || echo blocked");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("blocked"),
            "the read-only root let a write through"
        );
        assert!(!std::path::Path::new("/coderex-should-not-exist").exists());

        // Network is off by default, so the namespace has no interface but loopback.
        let out = run("cut -d: -f1 /proc/net/dev");
        let interfaces = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            !interfaces.contains("eth0"),
            "the network namespace was not created: {interfaces}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The syscall allowlist, exercised on a real process.
    ///
    /// Only meaningful when the wrapper is **not** in force — with `bwrap` the
    /// sandbox is the mount namespace, and `prepare` deliberately skips the
    /// seccomp table (the wrapper itself needs the syscalls the table denies).
    /// So this is skipped when `bwrap` is in force; run it with
    /// `CODEREX_BWRAP=0` to exercise the allowlist. A wrong table kills the
    /// child with SIGSYS, which is exactly what this asserts does not happen.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_seccomp_allowlist_lets_a_real_process_run() {
        if bubblewrap_in_force() {
            eprintln!("skipping: bubblewrap is in force; set CODEREX_BWRAP=0 to test the syscall table");
            return;
        }

        let handle = SandboxHandle::create(SandboxConfig::default()).unwrap();
        let mut cmd = std::process::Command::new("true");
        handle.prepare(&mut cmd).expect("pre_exec should install");
        let output = cmd.output().expect("spawn");
        assert!(
            output.status.success(),
            "the syscall allowlist killed a trivial process: {:?}",
            output.status
        );
    }

    #[test]
    fn bubblewrap_is_switched_off_only_by_an_explicit_negative() {
        for off in ["0", "false", "no"] {
            assert!(
                bubblewrap_disabled_by_env(Some(off)),
                "{off} should switch the wrapper off"
            );
        }
        for on in [None, Some("1"), Some("true"), Some("yes"), Some("")] {
            assert!(
                !bubblewrap_disabled_by_env(on),
                "{on:?} should leave the wrapper on"
            );
        }
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn bubblewrap_is_not_reported_on_non_linux() {
        assert!(!bubblewrap_available());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_sandbox_prepare_sets_pre_exec() {
        let config = SandboxConfig {
            max_open_files: Some(64),
            max_memory_bytes: Some(1024 * 1024 * 1024),
            max_cpu_seconds: Some(60),
            ..SandboxConfig::default()
        };
        let handle = SandboxHandle::create(config).unwrap();
        let mut cmd = std::process::Command::new("echo");
        // prepare installs pre_exec — if it compiles, the closure is valid
        handle.prepare(&mut cmd).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_sandbox_seccomp_filter_well_formed() {
        // We can't easily verify the filter is installed without forking,
        // but we can verify the SandboxHandle is created and prepare works.
        let handle = SandboxHandle::create(SandboxConfig::default()).unwrap();
        let mut cmd = std::process::Command::new("true");
        handle.prepare(&mut cmd).unwrap();
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn windows_sandbox_creates_job_object() {
        let config = SandboxConfig {
            max_memory_bytes: Some(512 * 1024 * 1024),
            max_cpu_seconds: Some(30),
            ..SandboxConfig::default()
        };
        let handle = SandboxHandle::create(config).unwrap();
        let mut cmd = std::process::Command::new("cmd");
        cmd.arg("/c").arg("echo hello");
        handle.prepare(&mut cmd).unwrap(); // no-op on Windows
        let child = cmd.spawn().unwrap();
        handle.post_spawn(&child).unwrap();
        let _ = child.wait_with_output();
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod seccomp_arch_tests {
    use super::seccomp_arch;

    /// A mismatch here is what the filter kills on, so the token must be the
    /// one matching the architecture the syscall numbers were taken from.
    #[test]
    fn audit_token_matches_the_syscall_table_architecture() {
        #[cfg(target_arch = "x86_64")]
        assert_eq!(seccomp_arch::AUDIT_ARCH, 0xC000_003E);
        #[cfg(target_arch = "aarch64")]
        assert_eq!(seccomp_arch::AUDIT_ARCH, 0xC000_00B7);
    }

    #[test]
    fn allowlist_is_non_empty_and_free_of_duplicates() {
        let allowed = seccomp_arch::ALLOWED;
        assert!(!allowed.is_empty());
        let unique: std::collections::HashSet<_> = allowed.iter().collect();
        assert_eq!(unique.len(), allowed.len(), "duplicate syscall in ALLOWED");
        assert!(
            allowed.iter().all(|&nr| nr >= 0),
            "syscall numbers are non-negative"
        );
    }

    /// The filter's default action is to kill, so a network syscall reaches the
    /// network only if it is explicitly listed. Listing one in both tables
    /// would make `allow_network: false` meaningless.
    #[test]
    fn network_syscalls_are_separate_from_the_base_allowlist() {
        let base: std::collections::HashSet<_> = seccomp_arch::ALLOWED.iter().collect();
        for nr in seccomp_arch::NETWORK {
            assert!(
                !base.contains(nr),
                "syscall {nr} is in both ALLOWED and NETWORK — the base profile \
                 would permit network access regardless of config"
            );
        }
        assert!(!seccomp_arch::NETWORK.is_empty());
    }
}
