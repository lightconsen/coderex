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
    /// The platform cannot provide the isolation the policy asks for.
    ///
    /// On Windows this means the AppContainer runtime is missing, the
    /// process-token call was refused, or the sandbox was asked for a
    /// capability this backend does not implement. Callers must treat it as a
    /// refusal, not as permission to run unconfined.
    #[error("sandbox isolation unavailable: {0}")]
    IsolationUnavailable(String),
    /// A Linux cgroup v2 limit could not be created or applied.
    #[error("failed to apply cgroup limit: {0}")]
    CgroupError(String),
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
// - Linux: `Kernel` when bubblewrap is in force — the command runs in a mount
//   namespace with a read-only root and only the configured paths writable —
//   or when the Landlock fallback is in force, where path-beneath rules grant
//   read/write/execute only beneath the configured paths. `None` only when
//   neither backend is available, where the sandbox is a seccomp syscall
//   allowlist plus rlimits and a wrapped process can still read and write
//   anything its user could.
// - Windows: `Kernel` when the AppContainer runtime is present — the child runs
//   under a container SID and path access is governed by DACLs. `None` on an
//   installation where the AppContainer APIs are unavailable; sandbox creation
//   then fails rather than degrading.
//
// The Linux and Windows answers are runtime answers: they depend on the
// installed kernel features/APIs, not just the compile target.

/// Report the filesystem confinement in force on this platform.
///
/// On Linux and Windows this is a runtime answer, not a compile-time one: it
/// depends on whether bubblewrap/Landlock is in force or the AppContainer APIs
/// exist.
#[must_use]
#[allow(clippy::missing_const_for_fn)] // the Linux and Windows bodies read the environment
pub fn filesystem_isolation() -> FilesystemIsolation {
    #[cfg(target_os = "macos")]
    {
        FilesystemIsolation::Kernel
    }
    #[cfg(target_os = "linux")]
    {
        if bubblewrap_in_force() || landlock_in_force() {
            FilesystemIsolation::Kernel
        } else {
            FilesystemIsolation::None
        }
    }
    #[cfg(target_os = "windows")]
    {
        if crate::sandbox_windows::app_container_available() {
            FilesystemIsolation::Kernel
        } else {
            FilesystemIsolation::None
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        FilesystemIsolation::None
    }
}

/// Whether the Windows AppContainer runtime is present.
///
/// Always `false` off Windows. Callers use it to decide whether a missing
/// confinement layer is a refusal or an environment they should report.
#[must_use]
// Windows calls a runtime API; on the other platforms it is a constant. The
// signature is shared, so it cannot be `const` everywhere.
#[allow(clippy::missing_const_for_fn)]
pub fn app_container_available() -> bool {
    #[cfg(target_os = "windows")]
    {
        crate::sandbox_windows::app_container_available()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
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

// ─── Memory ceiling: resource selection and best-effort attempt ─────────────

/// The rlimit a memory ceiling targets, parameterised by platform.
///
/// Darwin rejects `RLIMIT_AS` and does not enforce `RLIMIT_RSS`, so
/// `RLIMIT_DATA` is the only memory-shaped limit worth attempting there. Every
/// other Unix uses the address-space limit. Pure, so both answers can be
/// asserted on any host regardless of where the test runs; the production
/// Darwin path calls it with `true`.
#[cfg(unix)]
#[must_use]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const fn memory_limit_resource(darwin: bool) -> RlimitResource {
    if darwin {
        libc::RLIMIT_DATA as RlimitResource
    } else {
        libc::RLIMIT_AS as RlimitResource
    }
}

/// The type `setrlimit` accepts for its resource argument.
///
/// It is `c_int` on macOS/BSD and `u32` (`__rlimit_resource_t`) on Linux; the
/// helper used to return `c_int` unconditionally, which meant the Linux build
/// never compiled — the pipeline's clippy job caught it.
#[cfg(target_os = "linux")]
pub(crate) type RlimitResource = libc::__rlimit_resource_t;
/// See [`RlimitResource`].
#[cfg(not(target_os = "linux"))]
pub(crate) type RlimitResource = libc::c_int;

/// Apply `cur`/`max` to `resource`, returning the kernel's errno on rejection.
///
/// Split out from [`attempt_memory_limit`] so a test can force a rejection
/// (`cur > max`) and observe the error without changing the calling process's
/// real limits.
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn try_setrlimit(
    resource: RlimitResource,
    cur: libc::rlim_t,
    max: libc::rlim_t,
) -> Result<(), i32> {
    let limit = libc::rlimit {
        rlim_cur: cur,
        rlim_max: max,
    };
    // SAFETY: setrlimit only reads the struct passed to it and either installs
    // the limit or fails without changing the process.
    if unsafe { libc::setrlimit(resource, &raw const limit) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or_default())
    }
}

/// Attempt to install `bytes` as both the soft and hard memory ceiling.
///
/// Returns the kernel's errno when it refuses, so a rejection is observable
/// instead of being silently folded into the caller's state. A rejection is
/// not an error for spawning; see [`apply_darwin_memory_limit`].
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn attempt_memory_limit(darwin: bool, bytes: u64) -> Result<(), i32> {
    let value = bytes as libc::rlim_t;
    try_setrlimit(memory_limit_resource(darwin), value, value)
}

/// Try to install a Darwin memory ceiling in the **calling** process.
///
/// macOS rejects `RLIMIT_AS` and does not enforce `RLIMIT_RSS`, so
/// `RLIMIT_DATA` is the only memory-shaped limit worth attempting. It is
/// best-effort by design: modern Darwin (Monterey and later, including Sonoma
/// and the macOS 15 line) returns `EINVAL` for every useful finite value, just
/// as it does for `RLIMIT_AS`. The call therefore reports whether the kernel
/// accepted the limit (via [`attempt_memory_limit`]) instead of turning an
/// unsupported limit into a spawn failure. Callers install it from `pre_exec`,
/// where it is inherited by the sandbox wrapper and the program it execs.
///
/// A `true` result is not a full address-space cap: `RLIMIT_DATA` bounds the
/// data segment, not mappings the allocator creates with `mmap`. It is a
/// ceiling where the kernel honours it and a documented no-op where it does
/// not — see `docs/sandbox.md`.
#[cfg(target_os = "macos")]
#[must_use]
pub fn apply_darwin_memory_limit(bytes: u64) -> bool {
    attempt_memory_limit(true, bytes).is_ok()
}

// ─── Linux: Landlock path confinement ───────────────────────────────────────

/// The environment variable that switches the Landlock fallback off.
pub const LANDLOCK_ENV: &str = "CODEREX_LANDLOCK";

/// System roots a confined process needs just to start: the loader, its
/// libraries, name-service configuration, and the pseudo-filesystems most
/// programs touch.
///
/// These are read-only. They are added to every Landlock plan because a
/// path-beneath allowlist that named only the workspace would leave `execve`
/// unable to load libc.
pub const LANDLOCK_SYSTEM_READ_PATHS: &[&str] = &[
    "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/proc", "/dev", "/sys", "/run",
];

/// System directories a confined process may execute from. This mirrors the
/// exec paths the other backends name; a configured `allowed_exec_paths`
/// extends it.
///
/// The library directories are included because executing a dynamically
/// linked program makes the kernel open its ELF interpreter (for example
/// `/lib64/ld-linux-x86-64.so.2`) with `MAY_EXEC`. Without `EXECUTE` on the
/// loader path, `execve` fails with `EACCES` even though the program itself is
/// under an allowed directory.
pub const LANDLOCK_SYSTEM_EXEC_PATHS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/local/bin",
    "/lib",
    "/lib64",
    "/usr/lib",
    "/usr/lib64",
];

/// The path rules Landlock would install for a config.
///
/// Pure, so the policy is compiled and tested on every platform; only the
/// syscall layer in [`linux_impl`] is Linux-only. Paths are de-duplicated and
/// kept in a stable order. Whether a path exists is decided when the ruleset
/// is built, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandlockPlan {
    /// ABI version the access masks were chosen for.
    pub abi: u32,
    /// Paths granted `READ_FILE` and `READ_DIR`.
    pub read_paths: Vec<PathBuf>,
    /// Paths granted the write-side access rights.
    pub write_paths: Vec<PathBuf>,
    /// Paths granted `EXECUTE`.
    pub exec_paths: Vec<PathBuf>,
    /// Whether the ruleset handles the network access rights.
    ///
    /// Only possible from ABI 4. When true and [`SandboxConfig::allow_network`]
    /// is false, the ruleset handles `BIND_TCP`/`CONNECT_TCP` and adds no
    /// network rules, so both are denied.
    pub handle_network: bool,
}

/// Build the Landlock path plan for a config and ABI version.
#[must_use]
pub fn landlock_plan(config: &SandboxConfig, abi: u32) -> LandlockPlan {
    let mut read_paths = Vec::new();
    for path in LANDLOCK_SYSTEM_READ_PATHS {
        push_unique_path(&mut read_paths, PathBuf::from(path));
    }
    for path in &config.allowed_read_paths {
        push_unique_path(&mut read_paths, path.clone());
    }
    for path in &config.allowed_write_paths {
        // A path the child may write is also one it must be able to read.
        push_unique_path(&mut read_paths, path.clone());
    }
    for path in &config.additional_working_dirs {
        push_unique_path(&mut read_paths, path.clone());
    }
    // `/dev/null` is a write target for ordinary redirection (`2>/dev/null`);
    // `/dev` itself is only readable, so devices cannot be opened writable.
    push_unique_path(&mut read_paths, PathBuf::from("/dev/null"));

    let mut write_paths = Vec::new();
    for path in &config.allowed_write_paths {
        push_unique_path(&mut write_paths, path.clone());
    }
    for path in &config.additional_working_dirs {
        push_unique_path(&mut write_paths, path.clone());
    }
    push_unique_path(&mut write_paths, PathBuf::from("/dev/null"));

    let mut exec_paths = Vec::new();
    for path in LANDLOCK_SYSTEM_EXEC_PATHS {
        push_unique_path(&mut exec_paths, PathBuf::from(path));
    }
    for path in &config.allowed_exec_paths {
        push_unique_path(&mut exec_paths, path.clone());
    }

    LandlockPlan {
        abi,
        read_paths,
        write_paths,
        exec_paths,
        // Landlock's network rules arrived in ABI 4. Below that, the seccomp
        // table (installed alongside this fallback) is what denies the network.
        handle_network: abi >= 4 && !config.allow_network,
    }
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

/// The Landlock ABI version the kernel supports, or `None` when Landlock is
/// compiled out, disabled, or absent.
///
/// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` returns
/// the ABI version without creating anything.
#[must_use]
#[cfg(target_os = "linux")]
pub fn landlock_abi_version() -> Option<u32> {
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;

    // SAFETY: the version query takes no attribute and no size; both are zero
    // by contract. A negative return is a failure (ENOSYS/EOPNOTSUPP/...).
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if ret < 0 {
        None
    } else {
        u32::try_from(ret).ok()
    }
}

/// The Landlock ABI version — `None` outside Linux.
#[must_use]
#[cfg(not(target_os = "linux"))]
pub const fn landlock_abi_version() -> Option<u32> {
    None
}

/// Whether the kernel provides Landlock (ABI 1 or newer).
#[must_use]
#[cfg(target_os = "linux")]
pub fn landlock_available() -> bool {
    landlock_abi_version().is_some_and(|abi| abi >= 1)
}

/// Whether the kernel provides Landlock — always false outside Linux.
#[must_use]
#[cfg(not(target_os = "linux"))]
pub const fn landlock_available() -> bool {
    false
}

/// Whether an environment value explicitly switches Landlock off.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn landlock_disabled_by_env(value: Option<&str>) -> bool {
    matches!(value, Some("0" | "false" | "no"))
}

/// Whether Landlock is the filesystem backend in force.
///
/// It is the fallback used **when bubblewrap is not in force**: on a host
/// without `bwrap`, or with `CODEREX_BWRAP=0`. `CODEREX_LANDLOCK=0` (or
/// `false`/`no`) turns it off as well, which restores the old
/// seccomp-and-rlimits-only mode.
#[must_use]
#[cfg(target_os = "linux")]
pub fn landlock_in_force() -> bool {
    !bubblewrap_in_force()
        && landlock_available()
        && !landlock_disabled_by_env(std::env::var(LANDLOCK_ENV).ok().as_deref())
}

/// Whether Landlock is the filesystem backend in force — always false outside
/// Linux.
#[must_use]
#[cfg(not(target_os = "linux"))]
pub const fn landlock_in_force() -> bool {
    false
}

// ─── Linux: cgroup v2 limits ────────────────────────────────────────────────

/// A live cgroup v2 group carrying a memory and/or process ceiling.
///
/// This is the reusable helper the shell tools and command hooks use, and the
/// one a WASM helper can use without borrowing policy from either:
///
/// 1. [`CgroupMemoryLimit::create`] before spawning, with the configured
///    memory ceiling (and a process ceiling).
/// 2. Spawn the child.
/// 3. [`CgroupMemoryLimit::attach`] its pid as soon as it exists.
/// 4. Keep the value alive until the child exits; dropping it removes the
///    group.
///
/// Creation is deliberately fallible. Most unprivileged sessions cannot create
/// groups in the systemd-managed tree, and a read-only `/sys/fs/cgroup` cannot
/// carry one at all. Callers **warn and fall back to `RLIMIT_AS`**, never
/// pretend the cgroup limit is in force. `CODEREX_CGROUP_ROOT` names a
/// writable delegated tree for containers and tests.
#[cfg(target_os = "linux")]
pub mod cgroup {
    use super::SandboxError;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Environment override for the cgroup v2 tree to create groups under.
    pub const CGROUP_ROOT_ENV: &str = "CODEREX_CGROUP_ROOT";

    /// Default process ceiling for a limited child. A shell command that
    /// forks without bound is a resource hazard even when its memory is
    /// bounded.
    pub const DEFAULT_PIDS_MAX: u64 = 256;

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    /// A live cgroup v2 group. Dropping it removes the directory (best effort).
    #[derive(Debug)]
    pub struct CgroupMemoryLimit {
        dir: PathBuf,
    }

    impl CgroupMemoryLimit {
        /// Create a group and write `memory.max` and/or `pids.max`.
        ///
        /// Tries the process's own cgroup first, then the cgroup v2 mount
        /// root. A candidate that is not writable, or whose parent has not
        /// delegated the needed controllers, is reported in the error the
        /// caller logs on the way to the rlimit-only fallback.
        ///
        /// # Errors
        ///
        /// Returns [`SandboxError::CgroupError`] when no candidate tree can be
        /// created or configured. The caller should warn and continue.
        pub fn create(
            memory_max_bytes: Option<u64>,
            pids_max: Option<u64>,
        ) -> Result<Self, SandboxError> {
            if memory_max_bytes.is_none() && pids_max.is_none() {
                return Err(SandboxError::CgroupError(
                    "no memory or process limit was configured".to_string(),
                ));
            }

            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let name = cgroup_name(std::process::id(), id);
            let mut reasons = Vec::new();

            for parent in candidate_parents() {
                if !parent.join("cgroup.controllers").is_file() {
                    reasons.push(format!("{} is not a cgroup v2 tree", parent.display()));
                    continue;
                }

                let dir = parent.join(&name);
                match std::fs::create_dir(&dir) {
                    Ok(()) => {}
                    Err(error) => {
                        reasons.push(format!("{}: {error}", dir.display()));
                        continue;
                    }
                }

                match configure(&dir, memory_max_bytes, pids_max) {
                    Ok(()) => return Ok(Self { dir }),
                    Err(error) => {
                        // Do not leave a half-configured group behind.
                        let _ = std::fs::remove_dir(&dir);
                        reasons.push(format!("{}: {error}", dir.display()));
                    }
                }
            }

            Err(SandboxError::CgroupError(format!(
                "no writable cgroup v2 tree with the needed controllers ({}); set {} to a \
                 delegated writable tree, or accept the RLIMIT_AS fallback",
                reasons.join("; "),
                CGROUP_ROOT_ENV
            )))
        }

        /// Add a spawned child to the group.
        ///
        /// # Errors
        ///
        /// Returns [`SandboxError::CgroupError`] when `cgroup.procs` cannot be
        /// written. The child is already running, so callers warn rather than
        /// treating this as a spawn failure.
        pub fn attach(&self, pid: u32) -> Result<(), SandboxError> {
            std::fs::write(self.dir.join("cgroup.procs"), pid.to_string()).map_err(|error| {
                SandboxError::CgroupError(format!(
                    "could not add pid {pid} to {}: {error}",
                    self.dir.display()
                ))
            })
        }

        /// The cgroup directory, for logging and tests.
        #[must_use]
        pub fn path(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for CgroupMemoryLimit {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir(&self.dir) {
                tracing::debug!(
                    cgroup = %self.dir.display(),
                    %error,
                    "could not remove cgroup; a child may still be attached"
                );
            }
        }
    }

    /// The `0::<path>` line of a `/proc/self/cgroup` body.
    ///
    /// cgroup v2 has a single unified hierarchy, so the only relevant line is
    /// the one with an empty controller list.
    pub(crate) fn current_cgroup_from_proc(content: &str) -> Option<&str> {
        content.lines().find_map(|line| line.strip_prefix("0::"))
    }

    /// A group name that cannot collide with a concurrent process or group.
    pub(crate) fn cgroup_name(pid: u32, id: u64) -> String {
        format!("coderex-{pid}-{id}")
    }

    /// Candidate parent directories, most specific first.
    fn candidate_parents() -> Vec<PathBuf> {
        if let Some(root) = std::env::var_os(CGROUP_ROOT_ENV) {
            return vec![PathBuf::from(root)];
        }

        let mount = Path::new("/sys/fs/cgroup");
        let mut parents = Vec::new();

        if let Ok(content) = std::fs::read_to_string("/proc/self/cgroup") {
            if let Some(relative) = current_cgroup_from_proc(&content) {
                let path = if relative == "/" {
                    mount.to_path_buf()
                } else {
                    mount.join(relative.trim_start_matches('/'))
                };
                if path.exists() {
                    parents.push(path);
                }
            }
        }
        if !parents.iter().any(|path| path == mount) {
            parents.push(mount.to_path_buf());
        }
        parents
    }

    /// Turn on the controllers a child group needs, then write its limits.
    fn configure(
        dir: &Path,
        memory_max_bytes: Option<u64>,
        pids_max: Option<u64>,
    ) -> Result<(), std::io::Error> {
        let wanted: Vec<&str> = match (memory_max_bytes.is_some(), pids_max.is_some()) {
            (true, true) => vec!["memory", "pids"],
            (true, false) => vec!["memory"],
            (false, true) => vec!["pids"],
            (false, false) => Vec::new(),
        };

        // In cgroup v2 a controller exists in the child only when the parent
        // delegated it through `cgroup.subtree_control`. A non-delegated tree
        // makes this write fail; the caller falls back rather than pretending.
        let parent = dir
            .parent()
            .ok_or_else(|| std::io::Error::other("cgroup directory has no parent"))?;
        let enabled = std::fs::read_to_string(parent.join("cgroup.subtree_control"))
            .unwrap_or_default()
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let mut additions = String::new();
        for controller in &wanted {
            if !enabled.iter().any(|name| name == controller) {
                additions.push('+');
                additions.push_str(controller);
                additions.push(' ');
            }
        }
        if !additions.is_empty() {
            std::fs::write(parent.join("cgroup.subtree_control"), additions.trim_end())?;
        }

        if let Some(bytes) = memory_max_bytes {
            std::fs::write(dir.join("memory.max"), bytes.to_string())?;
            // OOM-kill the whole group rather than whichever process is
            // first; best effort, the file is absent on older kernels.
            let _ = std::fs::write(dir.join("memory.oom.group"), "1");
        }
        if let Some(pids) = pids_max {
            std::fs::write(dir.join("pids.max"), pids.to_string())?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use cgroup::CgroupMemoryLimit;

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
    /// On Windows: creates an AppContainer profile and its ACLs, plus a Job
    /// Object with limits. Fails with [`SandboxError::IsolationUnavailable`] if
    /// the AppContainer runtime is missing — it does *not* fall back to
    /// limits-only, because that would be a boundary the caller did not get.
    /// On unsupported platforms: returns a warning but proceeds.
    pub fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
        let inner = PlatformSandbox::create(config)?;
        Ok(Self { inner })
    }

    /// Create a sandbox that applies resource limits but no path isolation.
    ///
    /// Only Windows needs this, for callers (the hooks engine) that create
    /// their own child and cannot run the post-spawn AppContainer token
    /// attach. It must never be used where the caller believes the child is
    /// confined: on Windows this handle does not confine paths or network.
    /// Other platforms create their normal backend.
    pub fn create_limits_only(config: SandboxConfig) -> Result<Self, SandboxError> {
        let inner = PlatformSandbox::create_limits_only(config)?;
        Ok(Self { inner })
    }

    /// Prepare a `std::process::Command` to run inside the sandbox.
    ///
    /// - **Linux**: installs a `pre_exec` closure that applies rlimits,
    ///   Landlock path rules (when bubblewrap is not in force) and seccomp
    ///   filters in the child process before `execve`.
    /// - **macOS**: installs a best-effort `RLIMIT_DATA` ceiling when
    ///   [`SandboxConfig::max_memory_bytes`] is set. Modern Darwin rejects it;
    ///   the rejection is ignored and does not fail the spawn (the child can
    ///   observe the limit it actually inherited). Use
    ///   [`SandboxHandle::sandboxed_program`] to get the wrapper command.
    /// - **Windows**: starts the child suspended (`CREATE_SUSPENDED`) when a
    ///   security container is active. [`SandboxHandle::post_spawn`] must then
    ///   attach the AppContainer token, assign the Job Object and resume it; a
    ///   child whose post-spawn step is never run stays suspended.
    /// - **Unsupported**: no-op.
    pub fn prepare(&self, cmd: &mut std::process::Command) -> Result<(), SandboxError> {
        self.inner.prepare(cmd)
    }

    /// Complete sandbox setup after the child process has spawned.
    ///
    /// - **Windows**: attaches the AppContainer token to the suspended child,
    ///   assigns it to the Job Object, and resumes it. Any failure terminates
    ///   the child.
    /// - **Linux**: adds the child to the cgroup created for its memory limit,
    ///   when one was configured and could be created.
    /// - **Other platforms**: no-op.
    pub fn post_spawn(&self, child: &std::process::Child) -> Result<(), SandboxError> {
        self.inner.post_spawn(child)
    }

    /// Complete sandbox setup for a child identified by pid.
    ///
    /// The shell tools and hooks spawn through `tokio::process::Child`, whose
    /// only portable handle is its pid. On Linux this is where the cgroup
    /// attachment happens; elsewhere it is a no-op. A cgroup attaches after
    /// spawn because a process cannot be placed in a group before it exists.
    #[cfg(unix)]
    pub fn post_spawn_pid(&self, pid: u32) -> Result<(), SandboxError> {
        self.inner.post_spawn_pid(pid)
    }

    /// Complete sandbox setup for an asynchronously spawned child.
    ///
    /// The shell tools spawn through `tokio::process`, which hands out a raw
    /// handle rather than a `std::process::Child`. On Windows this attaches the
    /// AppContainer token to the suspended child, assigns the Job Object, and
    /// resumes it. Without a caller there was no caller at all: the Job Object
    /// was created and never assigned to anything.
    #[cfg(target_os = "windows")]
    pub fn post_spawn_raw(
        &self,
        handle: std::os::windows::io::RawHandle,
    ) -> Result<(), SandboxError> {
        self.inner.post_spawn_raw(handle)
    }

    /// Kill every process still owned by the sandbox.
    ///
    /// - **Windows**: `TerminateJobObject` on the sandbox's Job Object, so the
    ///   child and any descendants it started die together. Dropping the
    ///   handle does the same thing through `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`;
    ///   this method is for callers that want to stop the child now and keep
    ///   the handle.
    /// - **Other platforms**: a no-op. Unix callers that need a tree kill own
    ///   the process group (`coderex_core::process_tree::ProcessTree`).
    // `const` on Unix; Windows terminates a job, which is not a const call.
    #[allow(clippy::missing_const_for_fn)]
    pub fn terminate(&self) -> Result<(), SandboxError> {
        self.inner.terminate()
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
    // Boxed to keep the enum small: the Windows handle owns the policy, the
    // Job Object and the AppContainer state.
    #[cfg(target_os = "windows")]
    Windows(Box<WindowsSandbox>),
    Unsupported,
}

impl PlatformSandbox {
    fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
        #[cfg(target_os = "linux")]
        return LinuxSandbox::create(config).map(Self::Linux);

        #[cfg(target_os = "macos")]
        return MacOsSandbox::create(config).map(Self::MacOs);

        #[cfg(target_os = "windows")]
        return WindowsSandbox::create(config)
            .map(Box::new)
            .map(Self::Windows);

        #[allow(unreachable_code)]
        {
            tracing::warn!("Sandbox not supported on this platform; proceeding without isolation");
            Ok(Self::Unsupported)
        }
    }

    /// Create a handle for resource limits only.
    ///
    /// On Windows this deliberately skips the AppContainer; other platforms
    /// have no separate "limits only" mode and create their normal backend.
    fn create_limits_only(config: SandboxConfig) -> Result<Self, SandboxError> {
        #[cfg(target_os = "windows")]
        return WindowsSandbox::create_limits_only(config)
            .map(Box::new)
            .map(Self::Windows);

        #[allow(unreachable_code)]
        {
            Self::create(config)
        }
    }

    #[allow(unused_variables)] // only the Linux arm reads it
    fn prepare(&self, cmd: &mut std::process::Command) -> Result<(), SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Linux(s) => s.prepare(cmd),
            #[cfg(target_os = "macos")]
            Self::MacOs(s) => s.prepare(cmd),
            #[cfg(target_os = "windows")]
            Self::Windows(s) => s.prepare(cmd),
            Self::Unsupported => Ok(()),
        }
    }

    // `const` on macOS; the Windows arm attaches a token.
    #[allow(clippy::missing_const_for_fn)]
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

    // `const` on macOS; Linux attaches the cgroup.
    #[cfg(unix)]
    #[allow(clippy::missing_const_for_fn)]
    fn post_spawn_pid(&self, pid: u32) -> Result<(), SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Linux(s) => s.post_spawn_pid(pid),
            #[cfg(target_os = "macos")]
            Self::MacOs(s) => s.post_spawn_pid(pid),
            #[allow(unreachable_patterns)]
            _ => {
                let _ = pid;
                Ok(())
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn post_spawn_raw(&self, handle: std::os::windows::io::RawHandle) -> Result<(), SandboxError> {
        match self {
            Self::Windows(s) => s.post_spawn_raw(handle),
            _ => Ok(()),
        }
    }

    // Windows can fail; the other arms are no-ops, which is why clippy offers
    // to make this `const`. A single signature has to serve both.
    #[allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)]
    fn terminate(&self) -> Result<(), SandboxError> {
        match self {
            #[cfg(target_os = "windows")]
            Self::Windows(s) => s.terminate(),
            // Unix callers own the process group themselves (`ProcessTree`);
            // there is no sandbox-owned tree to kill here.
            #[allow(unreachable_patterns)]
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
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::sync::Arc;

    // Landlock userspace ABI. `libc` exposes the syscall numbers but not these
    // structs or access bits, so they are declared here from
    // `include/uapi/linux/landlock.h`.
    const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
    const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
    const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
    const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
    const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
    const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
    const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
    const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
    const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
    const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
    const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
    const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
    const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
    const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
    const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;

    const LANDLOCK_ACCESS_NET_BIND_TCP: u64 = 1 << 0;
    const LANDLOCK_ACCESS_NET_CONNECT_TCP: u64 = 1 << 1;

    const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
    const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;

    #[repr(C)]
    struct LandlockRulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    #[repr(C)]
    struct LandlockPathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// The filesystem rights this backend handles.
    ///
    /// Anything *not* listed here is unrestricted by Landlock, which is why
    /// `IOCTL_DEV` (ABI 5) is deliberately left out: confining it would break
    /// ordinary terminal handling unless every device path were granted.
    pub(super) const fn handled_fs_access(abi: u32) -> u64 {
        let mut access = LANDLOCK_ACCESS_FS_EXECUTE
            | LANDLOCK_ACCESS_FS_WRITE_FILE
            | LANDLOCK_ACCESS_FS_READ_FILE
            | LANDLOCK_ACCESS_FS_READ_DIR
            | LANDLOCK_ACCESS_FS_REMOVE_DIR
            | LANDLOCK_ACCESS_FS_REMOVE_FILE
            | LANDLOCK_ACCESS_FS_MAKE_CHAR
            | LANDLOCK_ACCESS_FS_MAKE_DIR
            | LANDLOCK_ACCESS_FS_MAKE_REG
            | LANDLOCK_ACCESS_FS_MAKE_SOCK
            | LANDLOCK_ACCESS_FS_MAKE_FIFO
            | LANDLOCK_ACCESS_FS_MAKE_BLOCK
            | LANDLOCK_ACCESS_FS_MAKE_SYM;
        if abi >= 2 {
            access |= LANDLOCK_ACCESS_FS_REFER;
        }
        if abi >= 3 {
            access |= LANDLOCK_ACCESS_FS_TRUNCATE;
        }
        access
    }

    const fn read_access() -> u64 {
        LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR
    }

    /// Which family of rights a rule grants.
    #[derive(Clone, Copy)]
    pub(super) enum AccessKind {
        Read,
        Write,
        Exec,
    }

    /// Rights applicable to a path of this type.
    ///
    /// A path-beneath rule on a **regular file** must not name directory-only
    /// rights (`READ_DIR`, `MAKE_*`, `REMOVE_*`, `REFER`): the kernel rejects
    /// such a rule with `EINVAL`. This matters for `/dev/null` and for any
    /// configured path that names a single binary.
    pub(super) const fn access_for(kind: AccessKind, is_dir: bool, abi: u32) -> u64 {
        match (kind, is_dir) {
            (AccessKind::Read, true) => read_access(),
            (AccessKind::Read, false) => LANDLOCK_ACCESS_FS_READ_FILE,
            (AccessKind::Write, true) => write_access(abi),
            (AccessKind::Write, false) => {
                let mut access = LANDLOCK_ACCESS_FS_WRITE_FILE;
                if abi >= 3 {
                    access |= LANDLOCK_ACCESS_FS_TRUNCATE;
                }
                access
            }
            (AccessKind::Exec, _) => exec_access(),
        }
    }

    pub(super) const fn write_access(abi: u32) -> u64 {
        let mut access = LANDLOCK_ACCESS_FS_WRITE_FILE
            | LANDLOCK_ACCESS_FS_REMOVE_DIR
            | LANDLOCK_ACCESS_FS_REMOVE_FILE
            | LANDLOCK_ACCESS_FS_MAKE_CHAR
            | LANDLOCK_ACCESS_FS_MAKE_DIR
            | LANDLOCK_ACCESS_FS_MAKE_REG
            | LANDLOCK_ACCESS_FS_MAKE_SOCK
            | LANDLOCK_ACCESS_FS_MAKE_FIFO
            | LANDLOCK_ACCESS_FS_MAKE_BLOCK
            | LANDLOCK_ACCESS_FS_MAKE_SYM;
        // `REFER` is what permits rename/link across directories; without it
        // in the write set, `mv` inside the workspace fails. It only exists
        // from ABI 2.
        if abi >= 2 {
            access |= LANDLOCK_ACCESS_FS_REFER;
        }
        // `TRUNCATE` is separate from `WRITE_FILE` from ABI 3; granting it
        // with the write paths keeps `> existing-file` working.
        if abi >= 3 {
            access |= LANDLOCK_ACCESS_FS_TRUNCATE;
        }
        access
    }

    const fn exec_access() -> u64 {
        LANDLOCK_ACCESS_FS_EXECUTE
    }

    /// Build a ruleset in the parent. The returned descriptor is inherited by
    /// the child, which calls `landlock_restrict_self` before exec.
    ///
    /// Rules are attached to inodes when added, so the path descriptors can be
    /// closed as soon as each rule is in place. A path that cannot be opened is
    /// skipped with a warning: a policy path that does not exist yet has no
    /// inode to grant, exactly as `bwrap` cannot bind a missing source.
    fn build_landlock_ruleset(plan: &super::LandlockPlan) -> Result<OwnedFd, std::io::Error> {
        let handled_access_net = if plan.handle_network {
            LANDLOCK_ACCESS_NET_BIND_TCP | LANDLOCK_ACCESS_NET_CONNECT_TCP
        } else {
            0
        };
        let attr = LandlockRulesetAttr {
            handled_access_fs: handled_fs_access(plan.abi),
            handled_access_net,
        };
        // Kernels before ABI 4 know only the first 8 bytes; passing zeroes in
        // the tail is explicitly allowed, but staying at the known size is
        // simpler and equally correct.
        let attr_size = if plan.abi >= 4 {
            std::mem::size_of::<LandlockRulesetAttr>()
        } else {
            std::mem::size_of::<u64>()
        };

        // SAFETY: `attr` is a valid pointer to `attr_size` initialized bytes;
        // flags are zero for a normal creation.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &raw const attr,
                attr_size,
                0u32,
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a non-negative return from landlock_create_ruleset is a new
        // owned file descriptor.
        let ruleset_fd = i32::try_from(ret)
            .map_err(|_| std::io::Error::other("ruleset descriptor out of range"))?;
        // SAFETY: landlock_create_ruleset returned an owned descriptor.
        let ruleset = unsafe { OwnedFd::from_raw_fd(ruleset_fd) };
        set_cloexec(ruleset.as_raw_fd());

        for (paths, kind) in [
            (&plan.read_paths, AccessKind::Read),
            (&plan.write_paths, AccessKind::Write),
            (&plan.exec_paths, AccessKind::Exec),
        ] {
            for path in paths {
                let is_dir = match std::fs::metadata(path) {
                    Ok(metadata) => metadata.is_dir(),
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "landlock: skipping a path whose type cannot be determined"
                        );
                        continue;
                    }
                };
                let allowed_access = access_for(kind, is_dir, plan.abi);

                let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
                    tracing::warn!(path = %path.display(), "landlock: path contains a NUL byte");
                    continue;
                };
                // SAFETY: `c_path` is NUL-terminated; the flags are read-only.
                let path_fd =
                    unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
                if path_fd < 0 {
                    let error = std::io::Error::last_os_error();
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "landlock: skipping a path that cannot be opened"
                    );
                    continue;
                }

                let rule = LandlockPathBeneathAttr {
                    allowed_access,
                    parent_fd: path_fd,
                };
                // SAFETY: `rule` is a valid path-beneath rule and `path_fd`
                // stays open until the rule has been added.
                let added = unsafe {
                    libc::syscall(
                        libc::SYS_landlock_add_rule,
                        ruleset.as_raw_fd(),
                        libc::c_ulong::from(LANDLOCK_RULE_PATH_BENEATH),
                        &raw const rule,
                        0u32,
                    )
                };
                let error = std::io::Error::last_os_error();
                // SAFETY: `path_fd` was opened above and is owned here.
                unsafe { libc::close(path_fd) };
                if added < 0 {
                    return Err(error);
                }
            }
        }

        Ok(ruleset)
    }

    fn set_cloexec(fd: i32) {
        // SAFETY: fcntl only reads the descriptor and the argument.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }

    /// Enter the ruleset in the calling (child) process.
    fn restrict_self(ruleset: i32) -> Result<(), std::io::Error> {
        // Landlock requires no_new_privs before it will restrict a process.
        // SAFETY: prctl with this option has no pointer arguments.
        let ret = unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `ruleset` is the descriptor built above and still open here.
        let ret = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Linux sandbox using bubblewrap or Landlock, plus seccomp + rlimits.
    pub struct LinuxSandbox {
        config: SandboxConfig,
        /// Whether commands are wrapped in `bubblewrap`.
        ///
        /// Decided once, at creation: both the warning and whether the syscall
        /// filter may be installed depend on it.
        use_bwrap: bool,
        /// Whether the Landlock fallback is in force.
        use_landlock: bool,
        /// The cgroup carrying this child's memory/pids limits, if one could
        /// be created. A failure to create one is logged and falls back to
        /// `RLIMIT_AS` rather than silently claiming a cgroup limit.
        cgroup: Option<super::CgroupMemoryLimit>,
    }

    impl LinuxSandbox {
        // On Linux every setup step falls back rather than failing; the
        // `Result` matches the other platform backends.
        #[allow(clippy::unnecessary_wraps)]
        pub fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
            let use_bwrap = super::bubblewrap_in_force();
            let use_landlock = super::landlock_in_force();
            let abi = super::landlock_abi_version().unwrap_or(0);

            // Say plainly what this sandbox is, in each case, because they
            // confine very different things.
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
            } else if use_landlock {
                tracing::info!(
                    abi,
                    "sandbox: filesystem isolation is enforced by Landlock — path-beneath rules \
                     grant read, write and execute only beneath the configured paths; reads of \
                     system roots stay allowed so the loader can start; network access is {}",
                    if config.allow_network {
                        "allowed (no network rules are added)"
                    } else if abi >= 4 {
                        "denied by Landlock network rules"
                    } else {
                        "denied by the seccomp syscall table"
                    }
                );
            } else if super::bubblewrap_available() {
                tracing::warn!(
                    "sandbox: filesystem isolation is NOT enforced on Linux — bubblewrap is \
                     installed but switched off by {}, and the Landlock fallback is unavailable \
                     or switched off; seccomp syscall filtering and rlimits are the only \
                     restrictions in force",
                    super::BWRAP_ENV
                );
            } else {
                tracing::warn!(
                    "sandbox: filesystem isolation is NOT enforced on Linux — bubblewrap (bwrap) \
                     is not installed and the kernel does not provide (or {} switched off) \
                     Landlock; seccomp syscall filtering and rlimits are the only restrictions \
                     in force",
                    super::LANDLOCK_ENV
                );
            }

            // The cgroup is created before the spawn so the child can be
            // attached the moment it exists. If the tree is not delegated or
            // is read-only, say so and keep the rlimit.
            let cgroup = config.max_memory_bytes.and_then(|bytes| {
                match super::CgroupMemoryLimit::create(
                    Some(bytes),
                    Some(super::cgroup::DEFAULT_PIDS_MAX),
                ) {
                    Ok(cgroup) => {
                        tracing::debug!(
                            cgroup = %cgroup.path().display(),
                            memory_bytes = bytes,
                            "sandbox: cgroup v2 memory limit prepared"
                        );
                        Some(cgroup)
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            memory_bytes = bytes,
                            "sandbox: cgroup v2 memory limit unavailable; the child falls back \
                             to RLIMIT_AS, which does not bound every mapping"
                        );
                        None
                    }
                }
            });

            Ok(Self {
                config,
                use_bwrap,
                use_landlock,
                cgroup,
            })
        }

        pub fn prepare(&self, cmd: &mut Command) -> Result<(), SandboxError> {
            let config = self.config.clone();
            let use_bwrap = self.use_bwrap;

            // Build the Landlock ruleset in the parent: adding rules opens
            // paths and allocates, neither of which belongs between fork and
            // exec. The child only has to enter the inherited ruleset.
            let ruleset = if self.use_landlock {
                let abi = super::landlock_abi_version().unwrap_or(0);
                let plan = super::landlock_plan(&config, abi);
                Some(Arc::new(build_landlock_ruleset(&plan).map_err(
                    |error| SandboxError::CreationError(format!("landlock ruleset: {error}")),
                )?))
            } else {
                None
            };

            let apply = move || -> Result<(), std::io::Error> {
                Self::apply_rlimits(&config)?;
                if let Some(ruleset) = &ruleset {
                    restrict_self(ruleset.as_raw_fd())?;
                    // The descriptor is CLOEXEC, but close it explicitly so a
                    // failing exec cannot leave it behind.
                    // SAFETY: the descriptor is owned by `ruleset` and this is
                    // its last use in the child.
                    unsafe { libc::close(ruleset.as_raw_fd()) };
                }
                // The syscall allowlist is not installed when bubblewrap wraps
                // the command: the filter would be inherited by the wrapper
                // itself, and mounting, unsharing and pivoting a root are
                // exactly the syscalls an allowlist for a shell refuses. What
                // the filter buys when it is in force — no network syscalls —
                // bubblewrap provides with a network namespace instead.
                //
                // With Landlock in force the seccomp table is still installed;
                // it is built and entered after the ruleset, so the Landlock
                // setup syscalls are not filtered.
                if !use_bwrap {
                    Self::apply_seccomp(config.allow_network)?;
                }
                Ok(())
            };
            // SAFETY: pre_exec runs in the child process before exec().
            // The closure captures only `config` (Clone) and an `Arc` holding
            // a ruleset descriptor; it references no parent-process state that
            // would be invalid after fork.
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

        pub fn post_spawn(&self, child: &std::process::Child) -> Result<(), SandboxError> {
            self.post_spawn_pid(child.id())
        }

        /// Attach a spawned child to the memory cgroup, when one exists.
        pub fn post_spawn_pid(&self, pid: u32) -> Result<(), SandboxError> {
            self.cgroup
                .as_ref()
                .map_or(Ok(()), |cgroup| cgroup.attach(pid))
        }

        // ─── rlimits ─────────────────────────────────────────────────────────

        fn apply_rlimits(config: &SandboxConfig) -> Result<(), std::io::Error> {
            use libc::{RLIMIT_AS, RLIMIT_CPU, RLIMIT_NOFILE, prlimit, rlimit};

            if let Some(max_files) = config.max_open_files {
                let rl = rlimit {
                    rlim_cur: max_files,
                    rlim_max: max_files,
                };
                let ret = unsafe { prlimit(0, RLIMIT_NOFILE, &raw const rl, std::ptr::null_mut()) };
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            if let Some(max_mem) = config.max_memory_bytes {
                let rl = rlimit {
                    rlim_cur: max_mem,
                    rlim_max: max_mem,
                };
                let ret = unsafe { prlimit(0, RLIMIT_AS, &raw const rl, std::ptr::null_mut()) };
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            if let Some(max_cpu) = config.max_cpu_seconds {
                let rl = rlimit {
                    rlim_cur: max_cpu,
                    rlim_max: max_cpu,
                };
                let ret = unsafe { prlimit(0, RLIMIT_CPU, &raw const rl, std::ptr::null_mut()) };
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

            // The vector is filled in two places — the arch prologue before
            // the syscall list is built, and the syscall pairs after it — so
            // start it with the prologue.
            let mut filter: Vec<sock_filter> = vec![
                // Load `seccomp_data.arch`, which is at offset **4** — the
                // struct is `{ nr: i32, arch: u32, ip: u64, args: [u64; 6] }`.
                // The two offsets used to be the other way round, so the
                // filter compared the syscall *number* against the
                // architecture token, never matched, and killed every process
                // at its first syscall.
                sock_filter {
                    code: BPF_LD + BPF_W + BPF_ABS,
                    jt: 0,
                    jf: 0,
                    k: 4,
                },
                // If the arch matches, skip the kill; otherwise fall through
                // to it. `jt`/`jf` are offsets from the *next* instruction,
                // so matching has to jump over the kill.
                sock_filter {
                    code: BPF_JMP + BPF_JEQ + BPF_K,
                    jt: 1,
                    jf: 0,
                    k: super::seccomp_arch::AUDIT_ARCH,
                },
                sock_filter {
                    code: BPF_RET + BPF_K,
                    jt: 0,
                    jf: 0,
                    k: SECCOMP_RET_KILL,
                },
                // Load `seccomp_data.nr`, the syscall number, at offset 0.
                sock_filter {
                    code: BPF_LD + BPF_W + BPF_ABS,
                    jt: 0,
                    jf: 0,
                    k: 0,
                },
            ];

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
                    k: u32::try_from(nr).unwrap_or(u32::MAX),
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
                len: u16::try_from(filter.len()).expect("seccomp filter length fits in u16"),
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
                    (&raw const prog) as libc::c_ulong,
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
        // On Linux every setup step falls back rather than failing; the
        // `Result` matches the other platform backends.
        #[allow(clippy::unnecessary_wraps)]
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

        /// Install the best-effort memory ceiling, if one is configured.
        ///
        /// Modern Darwin rejects `RLIMIT_DATA` with `EINVAL`, so the result is
        /// deliberately ignored: an unsupported limit must not turn a spawn
        /// into a failure. Where the kernel honours it, the limit is in place
        /// before the wrapper and the program it execs.
        // The result is always `Ok`: Darwin's rejection of the limit is
        // deliberately ignored. The signature matches the other backends.
        #[allow(clippy::unnecessary_wraps)]
        pub fn prepare(&self, cmd: &mut std::process::Command) -> Result<(), SandboxError> {
            use std::os::unix::process::CommandExt;

            if let Some(bytes) = self.config.max_memory_bytes {
                // SAFETY: pre_exec runs in the child between fork and exec and
                // only calls setrlimit, which is async-signal-safe.
                unsafe {
                    cmd.pre_exec(move || {
                        let _ = super::apply_darwin_memory_limit(bytes);
                        Ok(())
                    });
                }
            }
            Ok(())
        }

        // No-op on macOS; the signature matches the other backends.
        #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
        pub const fn post_spawn(&self, _child: &std::process::Child) -> Result<(), SandboxError> {
            Ok(())
        }

        // No-op on macOS; the signature matches the other backends.
        #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
        pub const fn post_spawn_pid(&self, _pid: u32) -> Result<(), SandboxError> {
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

// ─── Windows: AppContainer + Job Objects ────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::{SandboxConfig, SandboxError};
    use crate::process_tree::{JobLimits, ProcessTree};
    use crate::sandbox_windows::{AppContainer, app_container_available};
    use std::os::windows::io::RawHandle;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, TerminateProcess};

    /// Windows sandbox: an AppContainer for paths/network plus a Job Object for
    /// resource and UI limits.
    ///
    /// The AppContainer is optional only for the `limits_only` constructor,
    /// which exists for hook processes: the hooks engine creates its own child
    /// and cannot run the post-spawn token attach. A handle built by the normal
    /// [`Self::create`] always has a container; if one cannot be made, creation
    /// fails rather than silently degrading to limits-only.
    ///
    /// The Job Object is a [`ProcessTree`], so it is anonymous (not the shared
    /// named job this used to open) and carries
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: **dropping the sandbox kills every
    /// process still in the job**, including descendants. Callers keep the
    /// handle alive for the child's lifetime.
    pub struct WindowsSandbox {
        job: ProcessTree,
        container: Option<AppContainer>,
        /// A private directory the container may write in, with a Low
        /// integrity label, exported to the child as `TEMP`/`TMP`/`TMPDIR`.
        ///
        /// The user's shared temp tree is deliberately not granted: lowering
        /// its label would let every Low-integrity process on the machine
        /// write there, and leaving its label alone means the container cannot
        /// write there at all.
        temp_dir: Option<PathBuf>,
    }

    // SAFETY: `job` is a kernel handle with no thread affinity, and
    // `AppContainer` is `Send + Sync` (its SID is immutable). Moving the
    // sandbox between threads does not alias anything.
    unsafe impl Send for WindowsSandbox {}
    // SAFETY: the Win32 calls used through `&self` (`post_spawn_raw`) take the
    // handle by value and are safe to call from any thread.
    unsafe impl Sync for WindowsSandbox {}

    impl WindowsSandbox {
        /// Create a fully confined child: AppContainer plus Job Object.
        pub fn create(mut config: SandboxConfig) -> Result<Self, SandboxError> {
            if !app_container_available() {
                return Err(SandboxError::IsolationUnavailable(
                    "AppContainer APIs are unavailable on this Windows installation; \
                     refusing to run under resource limits alone"
                        .into(),
                ));
            }
            // A shell needs somewhere to write temporary files. Give it a
            // private, Low-labelled directory rather than opening up the
            // user's shared temp tree.
            let temp_dir = create_private_temp_dir()?;
            if let Some(dir) = &temp_dir {
                config.allowed_write_paths.push(dir.clone());
                config.additional_working_dirs.push(dir.clone());
            }
            let container = match AppContainer::create(&config) {
                Ok(container) => container,
                Err(error) => {
                    if let Some(dir) = &temp_dir {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                    return Err(error);
                }
            };
            Self::with_container(config, Some(container), temp_dir)
        }

        /// Create a handle that applies only the Job Object limits.
        ///
        /// There is deliberately no AppContainer here: a caller that cannot
        /// perform the post-spawn token attach must not be given a handle that
        /// pretends to confine paths. It exists for hooks, which still fail
        /// closed when they need a security boundary (see `hooks::sandbox`).
        pub fn create_limits_only(config: SandboxConfig) -> Result<Self, SandboxError> {
            Self::with_container(config, None, None)
        }

        fn with_container(
            config: SandboxConfig,
            container: Option<AppContainer>,
            temp_dir: Option<PathBuf>,
        ) -> Result<Self, SandboxError> {
            // One anonymous job per sandbox. It used to be the named
            // `CodeRexSandboxJob`, which every handle in the process (and any
            // other process on the machine) shared: the last `SetInformation`
            // won, and terminating one sandbox killed every other one's child.
            let job = ProcessTree::create(JobLimits {
                max_cpu_seconds: config.max_cpu_seconds,
                max_memory_bytes: config.max_memory_bytes,
                kill_on_drop: true,
                ui_restrictions: true,
            })
            .map_err(|error| {
                SandboxError::CreationError(format!("CreateJobObject failed: {error}"))
            })?;

            Ok(Self {
                job,
                container,
                temp_dir,
            })
        }

        /// Ask `std::process::Command` to stop the child before its first
        /// instruction, so the AppContainer token can be attached.
        pub fn prepare(&self, cmd: &mut std::process::Command) -> Result<(), SandboxError> {
            if self.container.is_some() {
                cmd.creation_flags(CREATE_SUSPENDED);
            }
            // Point the child at the writable temp directory. The inherited
            // values name the user's temp tree, which the container cannot
            // write in by design.
            if let Some(dir) = &self.temp_dir {
                cmd.env("TEMP", dir).env("TMP", dir).env("TMPDIR", dir);
            }
            Ok(())
        }

        /// Describe the container's access to `path`, for test failures.
        ///
        /// The Windows runtime job can only be debugged from its logs, so a
        /// failing confinement assertion includes this string: it separates a
        /// DACL that was never applied from a Low-integrity token blocked by a
        /// Medium-integrity label.
        pub fn diagnose(&self, path: &std::path::Path) -> String {
            match &self.container {
                Some(container) => crate::sandbox_windows::diagnose_path(path, container.sid()),
                None => "no AppContainer (limits-only sandbox)".to_string(),
            }
        }

        /// Kill every process still in the sandbox's job.
        ///
        /// This is the tree kill `Drop` performs as well, exposed so a caller
        /// can stop a long-running child without waiting for its own teardown.
        pub fn terminate(&self) -> Result<(), SandboxError> {
            if self.job.terminate() {
                Ok(())
            } else {
                Err(SandboxError::CreationError(
                    "TerminateJobObject failed or the job was already empty".to_string(),
                ))
            }
        }

        pub fn post_spawn(&self, child: &std::process::Child) -> Result<(), SandboxError> {
            use std::os::windows::io::AsRawHandle;
            self.post_spawn_raw(child.as_raw_handle())
        }

        /// Attach the AppContainer token to a still-suspended child.
        ///
        /// A failure terminates the child: it was created suspended precisely
        /// so it could not run outside the container, and reporting the error
        /// while letting it run would defeat that.
        fn attach_container(&self, handle: HANDLE) -> Result<(), SandboxError> {
            let Some(container) = &self.container else {
                return Ok(());
            };
            if let Err(error) = container.restrict_suspended(handle) {
                unsafe { TerminateProcess(handle, 1) };
                return Err(error);
            }
            Ok(())
        }

        /// Resume a child whose token was replaced by [`Self::attach_container`].
        fn resume_child(&self, handle: HANDLE) -> Result<(), SandboxError> {
            let Some(container) = &self.container else {
                return Ok(());
            };
            if let Err(error) = container.resume(handle) {
                unsafe { TerminateProcess(handle, 1) };
                return Err(error);
            }
            Ok(())
        }

        /// Finish setting up a spawned child: attach the AppContainer token,
        /// assign the Job Object, then resume.
        ///
        /// The child was created suspended by [`Self::prepare`]. Any failure
        /// before the resume terminates it — a child that ran even briefly
        /// outside the container would defeat the confinement, so this path is
        /// fail-closed. The job assignment stays after the token attach because
        /// once the token is replaced there is no need to touch the process as
        /// the user again.
        pub fn post_spawn_raw(&self, process: RawHandle) -> Result<(), SandboxError> {
            let handle = process as HANDLE;

            self.attach_container(handle)?;

            if let Err(error) = self.job.attach_raw(process) {
                let error = SandboxError::CreationError(format!(
                    "AssignProcessToJobObject failed: {error}"
                ));
                // A process still suspended never ran, so terminating it is
                // simply a refusal. A limits-only process is already running;
                // killing it would change the caller's semantics and the
                // caller reports the error either way.
                if self.container.is_some() {
                    unsafe { TerminateProcess(handle, 1) };
                }
                return Err(error);
            }

            self.resume_child(handle)?;
            Ok(())
        }
    }

    /// Create a unique private temp directory for one sandbox.
    fn create_private_temp_dir() -> Result<Option<PathBuf>, SandboxError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("coderex")
            .join("sandbox-tmp");
        let unique = format!(
            "{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        );
        let dir = base.join(unique);
        std::fs::create_dir_all(&dir).map_err(|error| {
            SandboxError::CreationError(format!(
                "could not create the sandbox temp directory {}: {error}",
                dir.display()
            ))
        })?;
        Ok(Some(dir))
    }

    impl Drop for WindowsSandbox {
        fn drop(&mut self) {
            // Kill the job first so nothing is still writing into the temp
            // directory while it is removed. `ProcessTree` would do this on
            // its own drop; doing it here just puts the order right.
            let _ = self.job.terminate();
            if let Some(dir) = self.temp_dir.take() {
                let _ = std::fs::remove_dir_all(dir);
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

    /// Whether the CI job requires a real AppContainer runtime.
    ///
    /// Without this, a runner image that lost the runtime would turn the
    /// confinement tests green by skipping them.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    fn require_app_container() -> bool {
        std::env::var("CODEREX_REQUIRE_APP_CONTAINER").as_deref() == Ok("1")
    }

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
        // A directory that shadows the shell must not be reported when the
        // real one is found earlier in PATH — the loader would not run it
        // either. Windows has no extensionless `sh`; `cmd.exe` is the program
        // that exists there.
        let program = if cfg!(windows) { "cmd.exe" } else { "sh" };
        let resolved = resolve_program(program, &std::env::temp_dir())
            .unwrap_or_else(|| panic!("{program} should be found on PATH"));
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
        // On Linux the answer depends on bubblewrap or Landlock, so the report
        // is pinned to what is actually in force rather than to the platform.
        #[cfg(target_os = "linux")]
        assert_eq!(
            filesystem_isolation(),
            if bubblewrap_in_force() || landlock_in_force() {
                FilesystemIsolation::Kernel
            } else {
                FilesystemIsolation::None
            }
        );
        // Windows reports what the AppContainer runtime can actually do, the
        // same runtime question as Linux. The arm used to fall through to
        // `None` even when the container was available.
        #[cfg(target_os = "windows")]
        assert_eq!(
            filesystem_isolation(),
            if crate::sandbox_windows::app_container_available() {
                FilesystemIsolation::Kernel
            } else {
                FilesystemIsolation::None
            }
        );
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        assert_eq!(filesystem_isolation(), FilesystemIsolation::None);
    }

    // ─── Landlock policy (pure) ─────────────────────────────────────────────

    fn landlock_config() -> SandboxConfig {
        SandboxConfig {
            allowed_read_paths: vec![PathBuf::from("/work/project")],
            allowed_write_paths: vec![PathBuf::from("/work/project")],
            allowed_exec_paths: vec![PathBuf::from("/opt/tool/bin")],
            additional_working_dirs: vec![PathBuf::from("/work/project")],
            allow_network: false,
            ..SandboxConfig::default()
        }
    }

    /// The configured paths are present once each, and system roots are added
    /// so the loader and the programs the sandbox starts can run.
    #[test]
    fn landlock_plan_deduplicates_and_includes_system_roots() {
        let plan = landlock_plan(&landlock_config(), 3);

        assert_eq!(
            plan.read_paths
                .iter()
                .filter(|path| path.as_path() == std::path::Path::new("/work/project"))
                .count(),
            1,
            "a path named in several fields must be granted once: {:?}",
            plan.read_paths
        );
        assert!(plan.read_paths.contains(&PathBuf::from("/usr")));
        assert!(plan.read_paths.contains(&PathBuf::from("/lib")));
        assert!(plan.write_paths.contains(&PathBuf::from("/work/project")));
        assert!(plan.write_paths.contains(&PathBuf::from("/dev/null")));
        assert!(plan.exec_paths.contains(&PathBuf::from("/bin")));
        assert!(plan.exec_paths.contains(&PathBuf::from("/opt/tool/bin")));
    }

    /// Landlock's network rules exist from ABI 4; below that the seccomp table
    /// is what denies the network, so the plan must not claim a network rule.
    #[test]
    fn landlock_network_rules_are_only_requested_from_abi_4() {
        assert!(!landlock_plan(&landlock_config(), 3).handle_network);
        assert!(landlock_plan(&landlock_config(), 4).handle_network);

        let mut network_config = landlock_config();
        network_config.allow_network = true;
        assert!(
            !landlock_plan(&network_config, 5).handle_network,
            "a command that may use the network must not have its connect/bind rights handled"
        );
    }

    /// ABI-dependent rights: `REFER` needs ABI 2 and `TRUNCATE` ABI 3. Asking
    /// an older kernel for them makes `landlock_create_ruleset` fail with
    /// EINVAL, so the masks must not include them there.
    #[test]
    fn landlock_access_masks_follow_the_abi() {
        #[cfg(target_os = "linux")]
        {
            use linux_impl::{handled_fs_access, write_access};
            const REFER: u64 = 1 << 13;
            const TRUNCATE: u64 = 1 << 14;

            assert_eq!(handled_fs_access(1) & REFER, 0);
            assert_eq!(handled_fs_access(1) & TRUNCATE, 0);
            assert_ne!(handled_fs_access(2) & REFER, 0);
            assert_eq!(handled_fs_access(2) & TRUNCATE, 0);
            assert_ne!(handled_fs_access(3) & TRUNCATE, 0);
            assert_eq!(write_access(1) & REFER, 0);
            assert_ne!(write_access(2) & REFER, 0);
            assert_eq!(write_access(2) & TRUNCATE, 0);
            assert_ne!(write_access(3) & TRUNCATE, 0);
        }
    }

    /// A path-beneath rule on a regular file must not name directory-only
    /// rights: the kernel rejects that with `EINVAL`. `/dev/null` is the case
    /// every shell hits.
    #[test]
    #[cfg(target_os = "linux")]
    fn landlock_file_rules_omit_directory_only_rights() {
        use linux_impl::{AccessKind, access_for};

        const READ_DIR: u64 = 1 << 3;
        const MAKE_REG: u64 = 1 << 8;
        const REFER: u64 = 1 << 13;
        const TRUNCATE: u64 = 1 << 14;
        const WRITE_FILE: u64 = 1 << 1;
        const READ_FILE: u64 = 1 << 2;

        let file_read = access_for(AccessKind::Read, false, 3);
        assert_eq!(file_read, READ_FILE);
        assert_eq!(file_read & READ_DIR, 0);

        let file_write = access_for(AccessKind::Write, false, 3);
        assert_ne!(file_write & WRITE_FILE, 0);
        assert_ne!(file_write & TRUNCATE, 0);
        assert_eq!(file_write & MAKE_REG, 0);
        assert_eq!(file_write & REFER, 0);

        let dir_write = access_for(AccessKind::Write, true, 3);
        assert_ne!(dir_write & MAKE_REG, 0);
        assert_ne!(dir_write & REFER, 0);
    }

    #[test]
    fn landlock_toggle_is_explicit() {
        for off in ["0", "false", "no"] {
            assert!(landlock_disabled_by_env(Some(off)));
        }
        for on in [None, Some("1"), Some("true"), Some("yes"), Some("")] {
            assert!(!landlock_disabled_by_env(on), "{on:?}");
        }
    }

    // ─── cgroup policy (pure) ───────────────────────────────────────────────

    #[test]
    #[cfg(target_os = "linux")]
    fn cgroup_name_is_unique_per_pid_and_creation() {
        assert_eq!(cgroup::cgroup_name(42, 0), "coderex-42-0");
        assert_eq!(cgroup::cgroup_name(42, 1), "coderex-42-1");
        assert_ne!(cgroup::cgroup_name(42, 1), cgroup::cgroup_name(43, 1));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn proc_self_cgroup_uses_the_unified_line() {
        assert_eq!(
            cgroup::current_cgroup_from_proc(
                "12:pids:/user.slice\n0::/user.slice/user-1000.slice/session-2.scope\n"
            ),
            Some("/user.slice/user-1000.slice/session-2.scope")
        );
        assert_eq!(cgroup::current_cgroup_from_proc("0::/\n"), Some("/"));
        assert_eq!(cgroup::current_cgroup_from_proc(""), None);
    }

    // ─── memory ceiling: resource selection and best-effort attempt ──────────

    /// The resource choice is a pure function of the platform, so both
    /// branches can be pinned on any Unix host.
    #[test]
    #[cfg(unix)]
    fn memory_limit_resource_is_data_on_darwin_and_as_elsewhere() {
        assert_eq!(memory_limit_resource(true), libc::RLIMIT_DATA);
        assert_eq!(memory_limit_resource(false), libc::RLIMIT_AS);

        // The platform's own answer matches the OS this test runs on.
        let selected = memory_limit_resource(cfg!(target_os = "macos"));
        if cfg!(target_os = "macos") {
            assert_eq!(selected, libc::RLIMIT_DATA);
        } else {
            assert_eq!(selected, libc::RLIMIT_AS);
        }
    }

    /// The helper must never turn an unsupported kernel limit into a panic or
    /// an error, and a rejection must remain observable to the caller.
    #[test]
    #[cfg(target_os = "macos")]
    fn darwin_memory_limit_is_best_effort() {
        // `u64::MAX` (infinity) cannot lower this process's own ceiling when
        // accepted, so running the attempt in the test process is safe either
        // way. Modern Darwin refuses it; another kernel may accept it.
        let attempt = attempt_memory_limit(true, u64::MAX);
        assert_eq!(memory_limit_resource(true), libc::RLIMIT_DATA);
        if let Err(errno) = attempt {
            assert!(
                matches!(errno, libc::EINVAL | libc::EPERM | libc::ENOTSUP),
                "unexpected rejection errno {errno}: {}",
                std::io::Error::from_raw_os_error(errno)
            );
        }
        // The public bool API reports exactly what the attempt saw; it does
        // not panic, abort, or hide a rejection.
        assert_eq!(apply_darwin_memory_limit(u64::MAX), attempt.is_ok());

        // A rejection is observable rather than swallowed: `cur > max` is an
        // invalid request that changes nothing, but its errno comes back.
        assert_eq!(
            try_setrlimit(libc::RLIMIT_DATA, 1, 0),
            Err(libc::EINVAL),
            "an invalid limit must be reported, not silently accepted"
        );
    }

    /// A rejected ceiling must not abort the spawn, and the caller must be
    /// able to observe that it was not applied.
    ///
    /// The limit is installed from `pre_exec`, so its real outcome can only be
    /// observed in the child: `/bin/sh` reports the inherited `RLIMIT_DATA`.
    /// On Darwin versions that reject the limit this is `unlimited`; where the
    /// kernel honours it, it is the configured 64 MiB (`65536` KiB). Either
    /// outcome is a successful spawn.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_prepare_with_a_rejected_memory_limit_still_spawns() {
        let handle = SandboxHandle::create(SandboxConfig {
            max_memory_bytes: Some(64 * 1024 * 1024),
            ..SandboxConfig::default()
        })
        .unwrap();
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("ulimit -d");
        handle.prepare(&mut cmd).expect("prepare must not fail");
        let output = cmd.output().expect("spawn must not fail");
        assert!(
            output.status.success(),
            "the child did not run: {:?}",
            output.status
        );

        let inherited = String::from_utf8_lossy(&output.stdout);
        let inherited = inherited.trim();
        assert!(
            inherited == "unlimited" || inherited == "65536",
            "the child should report the limit it inherited, got {inherited:?}"
        );
        if inherited == "unlimited" {
            // Pre-exec's attempt was rejected. Confirm the kernel really does
            // refuse this value with a throwaway child, so a silent failure to
            // install the hook is not mistaken for a rejected limit. The probe
            // starts without the pre-exec hook, so it also cannot lower this
            // test process's limits.
            let probe = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg("ulimit -d 65536")
                .stderr(std::process::Stdio::null())
                .status()
                .expect("the probe child must run");
            assert!(
                !probe.success(),
                "the child inherited no limit although the kernel accepts it"
            );
        }
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
            eprintln!(
                "skipping: bubblewrap is in force; set CODEREX_BWRAP=0 to test the syscall table"
            );
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

    /// The Landlock fallback, exercised on a real process.
    ///
    /// Skipped when bubblewrap is in force (it is the better backend and
    /// Landlock is not applied then), when the kernel has no Landlock, or when
    /// `CODEREX_LANDLOCK=0` switched it off.
    #[test]
    #[cfg(target_os = "linux")]
    fn landlock_fallback_confines_a_real_process() {
        if !landlock_in_force() {
            eprintln!(
                "skipping: Landlock is not in force (bwrap is in force, the kernel lacks \
                 Landlock, or CODEREX_LANDLOCK=0)"
            );
            return;
        }

        let root = std::env::temp_dir().join(format!("coderex-landlock-{}", std::process::id()));
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
            additional_working_dirs: vec![allowed.clone()],
            ..SandboxConfig::default()
        };
        let handle = SandboxHandle::create(config).unwrap();
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(format!(
            "cat {}/f.txt; cat {}/f.txt 2>/dev/null || echo read-denied; \
             echo ok > {}/new.txt; echo bad > {}/escape.txt 2>/dev/null || echo write-denied",
            allowed.display(),
            secret.display(),
            allowed.display(),
            secret.display()
        ));
        handle.prepare(&mut cmd).expect("prepare installs Landlock");

        let output = cmd.output().expect("spawn");
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let allowed_written = std::fs::read_to_string(allowed.join("new.txt")).unwrap_or_default();
        let escaped = secret.join("escape.txt").exists();
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            stdout.contains("ALLOWED"),
            "the allowed read failed: {stdout} {stderr}"
        );
        assert!(
            !stdout.contains("SECRET"),
            "a file outside the read rules was readable: {stdout}"
        );
        assert_eq!(
            allowed_written.trim(),
            "ok",
            "the allowed write failed: {stderr}"
        );
        assert!(
            !escaped,
            "a write outside the Landlock write rules succeeded"
        );
    }

    /// A cgroup limit is created with the configured values, and a spawned
    /// process can be attached. Skips with an explicit message when the tree
    /// is not writable/delegated (the normal unprivileged case).
    #[test]
    #[cfg(target_os = "linux")]
    fn cgroup_memory_limit_attaches_a_real_child() {
        let limit = match CgroupMemoryLimit::create(Some(64 * 1024 * 1024), Some(64)) {
            Ok(limit) => limit,
            Err(error) => {
                eprintln!("skipping: cgroup v2 limits are unavailable here: {error}");
                return;
            }
        };

        let memory_max = std::fs::read_to_string(limit.path().join("memory.max")).unwrap();
        let pids_max = std::fs::read_to_string(limit.path().join("pids.max")).unwrap();
        assert_eq!(memory_max.trim(), (64 * 1024 * 1024).to_string());
        assert_eq!(pids_max.trim(), "64");

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep should spawn");
        limit
            .attach(child.id())
            .expect("the child should be attachable");

        let membership =
            std::fs::read_to_string(format!("/proc/{}/cgroup", child.id())).unwrap_or_default();
        let name = limit
            .path()
            .file_name()
            .expect("the cgroup directory has a name")
            .to_string_lossy()
            .to_string();

        let _ = child.kill();
        let _ = child.wait();
        drop(limit);

        assert!(
            membership.contains(&name),
            "the child is not in {name}: {membership}"
        );
    }

    /// `memory.max` actually fires: a process that allocates past the ceiling
    /// is killed or gets `MemoryError`. Skipped when python3 or cgroups are
    /// unavailable.
    #[test]
    #[cfg(target_os = "linux")]
    fn cgroup_memory_limit_stops_a_memory_hog() {
        let python = std::process::Command::new("python3")
            .arg("--version")
            .output();
        if python.is_err() {
            eprintln!("skipping: python3 is not available to allocate memory");
            return;
        }
        let limit = match CgroupMemoryLimit::create(Some(64 * 1024 * 1024), Some(64)) {
            Ok(limit) => limit,
            Err(error) => {
                eprintln!("skipping: cgroup v2 limits are unavailable here: {error}");
                return;
            }
        };

        let mut child = std::process::Command::new("python3")
            .arg("-I")
            .arg("-c")
            .arg("a=[]\nwhile True:\n a.append(bytearray(8*1024*1024))")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("python3 should spawn");
        limit
            .attach(child.id())
            .expect("the child should be attachable");

        let output = child.wait_with_output().expect("python3 should exit");
        drop(limit);

        assert!(
            !output.status.success(),
            "a 64 MiB cgroup should stop an unbounded allocator, but it exited successfully"
        );
    }

    /// The path the shell tools use: create suspended, attach the AppContainer
    /// token, assign the Job Object, resume.
    ///
    /// Runs in the `test-windows` CI job (advisory until that job has a stable
    /// history). The container may write in the granted directory and must not
    /// write in an ungranted sibling.
    #[test]
    #[cfg(target_os = "windows")]
    fn windows_sandbox_confines_a_suspended_child() {
        if !crate::sandbox_windows::app_container_available() {
            // The CI job sets this so an image without the AppContainer
            // runtime fails instead of silently passing without coverage.
            assert!(
                !require_app_container(),
                "AppContainer is unavailable on this runner but \
                 CODEREX_REQUIRE_APP_CONTAINER=1"
            );
            // The backend refuses to run unconfined; that refusal is the
            // contract, so assert it rather than skipping silently.
            assert!(matches!(
                SandboxHandle::create(SandboxConfig::default()),
                Err(SandboxError::IsolationUnavailable(_))
            ));
            assert!(SandboxHandle::create_limits_only(SandboxConfig::default()).is_ok());
            return;
        }

        let base = std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let root = base.join(format!("coderex-sandbox-{}", std::process::id()));
        let allowed = root.join("allowed");
        let denied = root.join("denied");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&denied).unwrap();

        let config = SandboxConfig {
            allowed_read_paths: vec![allowed.clone()],
            allowed_write_paths: vec![allowed.clone()],
            additional_working_dirs: vec![allowed.clone()],
            max_memory_bytes: Some(512 * 1024 * 1024),
            max_cpu_seconds: Some(30),
            ..SandboxConfig::default()
        };
        let handle = SandboxHandle::create(config).unwrap();

        let inside = allowed.join("inside.txt");
        let outside = denied.join("outside.txt");
        // The temp probe separates "the container cannot write anywhere" from
        // "the policy path is wrong": `%TEMP%` points at the sandbox's private
        // directory, which it definitely granted.
        let script = format!(
            "echo TEMP=%TEMP% & echo probe> %TEMP%\\coderex-probe.txt & \
             echo inside> {} & echo outside> {}",
            inside.display(),
            outside.display()
        );
        let mut cmd = std::process::Command::new("cmd.exe");
        cmd.arg("/C")
            .arg(script)
            .current_dir(&allowed)
            .stdin(std::process::Stdio::null())
            // Inherited, not piped: a contained cmd.exe can leave a helper
            // holding the pipe open, and `wait_with_output` would then wait
            // forever even after cmd itself exited.
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
        handle.prepare(&mut cmd).unwrap();
        let mut child = cmd.spawn().unwrap();
        handle.post_spawn(&child).unwrap();
        let status = wait_bounded(&mut child, std::time::Duration::from_secs(30));

        // The runtime job can only be debugged from its logs, so the failure
        // carries what the DACL and the mandatory label actually look like,
        // what the container's token looks like, and what the command said.
        let detail = match &handle.inner {
            PlatformSandbox::Windows(sandbox) => sandbox.diagnose(&allowed),
            _ => String::new(),
        };
        let acl = std::process::Command::new("icacls")
            .arg(&allowed)
            .output()
            .map_or_else(
                |error| format!("<icacls failed: {error}>"),
                |out| String::from_utf8_lossy(&out.stdout).into_owned(),
            );
        let groups = run_in_container(&handle, &allowed, "whoami /groups");
        let probe = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("TEMP=").map(str::to_string))
            .map_or_else(
                || "<the command never printed TEMP>".to_string(),
                |dir| {
                    let path = std::path::Path::new(&dir).join("coderex-probe.txt");
                    format!("{} exists={}", path.display(), path.exists())
                },
            );
        assert!(
            inside.exists(),
            "the granted workspace was not writable (exit={:?})\n{detail}\n\
             temp probe: {probe}\nicacls:\n{acl}\ncontainer identity:\n{groups}",
            status.map(|s| s.code())
        );
        assert!(!outside.exists(), "a path outside the policy was writable");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Wait for a child, killing it and returning `None` after `limit`.
    #[cfg(target_os = "windows")]
    fn wait_bounded(
        child: &mut std::process::Child,
        limit: std::time::Duration,
    ) -> Option<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// Run `command` inside the sandbox and return its combined output.
    #[cfg(target_os = "windows")]
    fn run_in_container(handle: &SandboxHandle, cwd: &std::path::Path, command: &str) -> String {
        let path = std::env::temp_dir().join(format!(
            "coderex-whoami-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        let Ok(file) = std::fs::File::create(&path) else {
            return "<could not create the diagnostic file>".to_string();
        };
        let diagnostics_file = file.try_clone().expect("the file was just created");
        let mut cmd = std::process::Command::new("cmd.exe");
        cmd.arg("/C")
            .arg(command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            // A file, not a pipe: a contained helper cannot keep this open
            // past the child's exit.
            .stdout(std::process::Stdio::from(file))
            .stderr(std::process::Stdio::from(diagnostics_file));
        let Ok(()) = handle.prepare(&mut cmd) else {
            return "<prepare failed>".to_string();
        };
        let Ok(mut child) = cmd.spawn() else {
            return "<spawn failed>".to_string();
        };
        if let Err(error) = handle.post_spawn(&child) {
            return format!("<post_spawn failed: {error}>");
        }
        let text = if wait_bounded(&mut child, std::time::Duration::from_secs(15)).is_some() {
            std::fs::read_to_string(&path).unwrap_or_default()
        } else {
            "<the diagnostic command timed out>".to_string()
        };
        let _ = std::fs::remove_file(&path);
        text
    }
}

#[cfg(all(
    test,
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

    /// The filter's default action is `SECCOMP_RET_ERRNO` with `EPERM` (not
    /// kill), so a network syscall reaches the network only if it is explicitly
    /// listed. Listing one in both tables would make `allow_network: false`
    /// meaningless.
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
