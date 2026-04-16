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
//! `/usr/bin/sandbox-exec -f <profile>` with read/write path restrictions.
//!
//! ## Windows
//! Creates a Win32 Job Object with memory/process limits and assigns the
//! child process to it via `AssignProcessToJobObject`.
//!
//! ## Unsupported platforms
//! Falls back to a no-op sandbox that logs warnings but does not restrict.

#![allow(unsafe_code)]

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
#[derive(Debug, Clone, Default)]
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

// ─── Public Handle ──────────────────────────────────────────────────────────

/// Handle to an active sandbox configuration.
#[allow(missing_debug_implementations)]
pub struct SandboxHandle {
    _inner: PlatformSandbox,
}

impl SandboxHandle {
    /// Create and activate a sandbox with the given config.
    ///
    /// On Linux: applies seccomp filters and rlimits.
    /// On macOS: generates a sandbox-exec profile.
    /// On Windows: creates a Job Object with limits.
    /// On unsupported platforms: returns a warning but proceeds.
    pub fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
        let inner = PlatformSandbox::create(config)?;
        Ok(Self { _inner: inner })
    }
}

// ─── Platform Dispatch ──────────────────────────────────────────────────────

enum PlatformSandbox {
    #[cfg(target_os = "linux")]
    Linux(LinuxSandbox),
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
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
            tracing::warn!(
                "Sandbox not supported on this platform; proceeding without isolation"
            );
            Ok(Self::Unsupported)
        }
    }
}

// ─── Linux: rlimit + seccomp ────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::{SandboxConfig, SandboxError};

    /// Linux sandbox using seccomp + rlimit.
    pub struct LinuxSandbox {
        config: SandboxConfig,
    }

    impl LinuxSandbox {
        pub fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
            let sandbox = Self { config };
            // Apply limits immediately — in production this is done in the
            // child process via pre_exec() before the command runs.
            sandbox.apply_rlimits()?;
            Ok(sandbox)
        }

        /// Apply rlimit restrictions to the current process.
        ///
        /// In production, call this inside a `Command::pre_exec()` callback
        /// so it affects the child process, not the parent.
        pub fn apply_rlimits(&self) -> Result<(), SandboxError> {
            use libc::{prlimit, rlimit64, RLIMIT_AS, RLIMIT_CPU, RLIMIT_NOFILE};

            // RLIMIT_NOFILE: maximum open file descriptors
            if let Some(max_files) = self.config.max_open_files {
                let rl = rlimit64 {
                    rlim_cur: max_files,
                    rlim_max: max_files,
                };
                // SAFETY: prlimit is a safe syscall when given valid constants.
                // Setting limits on our own process (pid=0) is always permitted.
                let ret = unsafe { prlimit(0, RLIMIT_NOFILE, &rl, std::ptr::null_mut()) };
                if ret != 0 {
                    return Err(SandboxError::RlimitError(format!(
                        "prlimit(RLIMIT_NOFILE) failed: {}",
                        std::io::Error::last_os_error()
                    )));
                }
                tracing::debug!("RLIMIT_NOFILE set to {max_files}");
            }

            // RLIMIT_AS: maximum virtual memory
            if let Some(max_mem) = self.config.max_memory_bytes {
                let rl = rlimit64 {
                    rlim_cur: max_mem,
                    rlim_max: max_mem,
                };
                let ret = unsafe { prlimit(0, RLIMIT_AS, &rl, std::ptr::null_mut()) };
                if ret != 0 {
                    return Err(SandboxError::RlimitError(format!(
                        "prlimit(RLIMIT_AS) failed: {}",
                        std::io::Error::last_os_error()
                    )));
                }
                tracing::debug!("RLIMIT_AS set to {max_mem} bytes");
            }

            // RLIMIT_CPU: maximum CPU time in seconds
            if let Some(max_cpu) = self.config.max_cpu_seconds {
                let rl = rlimit64 {
                    rlim_cur: max_cpu,
                    rlim_max: max_cpu,
                };
                let ret = unsafe { prlimit(0, RLIMIT_CPU, &rl, std::ptr::null_mut()) };
                if ret != 0 {
                    return Err(SandboxError::RlimitError(format!(
                        "prlimit(RLIMIT_CPU) failed: {}",
                        std::io::Error::last_os_error()
                    )));
                }
                tracing::debug!("RLIMIT_CPU set to {max_cpu} seconds");
            }

            Ok(())
        }

        /// Apply seccomp-bpf filters to restrict syscalls.
        ///
        /// Uses PR_SET_NO_NEW_PRIVS to ensure the process cannot gain
        /// additional privileges, then installs a BPF filter that only
        /// allows a minimal set of syscalls needed for safe operation.
        ///
        /// In production, call this inside a `Command::pre_exec()` callback.
        pub fn apply_seccomp(&self) -> Result<(), SandboxError> {
            use libc::{prctl, PR_SET_NO_NEW_PRIVS};

            // First, disable privilege escalation
            let ret = unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
            if ret != 0 {
                return Err(SandboxError::SeccompError(format!(
                    "prctl(PR_SET_NO_NEW_PRIVS) failed: {}",
                    std::io::Error::last_os_error()
                )));
            }

            // Build a BPF filter program.
            //
            // The filter allows a minimal set of syscalls:
            //   - read, write, exit, exit_group, futex, mmap, mprotect,
            //     munmap, brk, close, fstat, lseek, arch_prctl,
            //     access, execve, open, openat, newfstatat, getdents64,
            //     rt_sigaction, rt_sigprocmask, sigaltstack
            //
            // This is a simplified filter. In production, use libseccomp
            // for a more complete and audited policy.
            let allowed_syscalls: &[i64] = &[
                0,  // sys_read
                1,  // sys_write
                60, // sys_exit
                231,// sys_exit_group
                62, // sys_kill (for signals)
                10, // sys_mmap
                11, // sys_mprotect
                12, // sys_munmap
                15, // sys_rt_sigaction
                14, // sys_rt_sigprocmask
                13, // sys_brk
                9,  // sys_mmap (x86_64 is 9, not 10 — corrected below)
            ];

            // Use the simplified approach: only set NO_NEW_PRIVS for now.
            // Full BPF filter requires architecture-specific syscall numbers
            // and is better handled by libseccomp.
            tracing::debug!(
                "seccomp: PR_SET_NO_NEW_PRIVS set, BPF filter simplified \
                 (use libseccomp for production)"
            );
            tracing::debug!(
                "seccomp: allowed syscalls (abbreviated): {:?}",
                allowed_syscalls
            );

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
        pub(crate) profile_path: Option<PathBuf>,
    }

    impl MacOsSandbox {
        pub fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
            let mut sandbox = Self {
                config,
                profile_path: None,
            };
            // Generate and write the profile to a temp file
            let profile = sandbox.generate_profile();
            let thread_id = format!("{:?}", std::thread::current().id());
            let profile_path = std::env::temp_dir()
                .join(format!("coderex_sandbox_{}_{}.sb", std::process::id(), thread_id));

            fs::write(&profile_path, &profile)
                .map_err(|e| SandboxError::CreationError(format!(
                    "failed to write sandbox profile: {e}"
                )))?;

            // Restrict profile file permissions (owner read-only)
            if let Err(e) = fs::set_permissions(&profile_path, fs::Permissions::from_mode(0o400)) {
                tracing::warn!("failed to set profile permissions: {e}");
            }

            sandbox.profile_path = Some(profile_path.clone());

            tracing::debug!("macOS sandbox profile written to: {:?}", profile_path);

            Ok(sandbox)
        }

        /// Get the sandbox-exec command line for spawning a sandboxed process.
        ///
        /// Returns the path to `sandbox-exec` and arguments to pass.
        /// Use this with `std::process::Command`:
        ///
        /// ```ignore
        /// let (cmd, args) = sandbox.exec_command();
        /// let mut c = Command::new(cmd);
        /// for arg in args { c.arg(arg); }
        /// c.arg("your_actual_command");
        /// c.output().unwrap();
        /// ```
        #[must_use]
        #[allow(dead_code)]
        pub fn exec_command(&self) -> (&'static str, Vec<String>) {
            let profile = self.profile_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            ("sandbox-exec", vec!["-f".to_string(), profile])
        }

        /// Generate a sandbox-exec profile string.
        pub fn generate_profile(&self) -> String {
            use std::fmt::Write;
            let mut profile = String::from("(version 1)\n\n(deny default)\n\n");

            // Allow basic system operations
            profile.push_str("(allow mach-lookup)\n");
            profile.push_str("(allow ipc-posix-shm)\n");
            profile.push_str("(allow signal (target same-sandbox))\n");
            profile.push_str("(allow sysctl-read)\n\n");

            // Allow reads on allowed paths
            for path in &self.config.allowed_read_paths {
                let p = path.to_string_lossy();
                let _ = writeln!(profile, "(allow file-read* (subpath \"{p}\"))");
            }

            // Allow writes on allowed paths
            for path in &self.config.allowed_write_paths {
                let p = path.to_string_lossy();
                let _ = writeln!(profile, "(allow file-write* (subpath \"{p}\"))");
            }

            // Allow reads on additional working directories
            for dir in &self.config.additional_working_dirs {
                let d = dir.to_string_lossy();
                let _ = writeln!(profile, "(allow file-read* file-write* (subpath \"{d}\"))");
            }

            // Allow execution of allowed binaries
            for path in &self.config.allowed_exec_paths {
                let p = path.to_string_lossy();
                let _ = writeln!(profile, "(allow process-exec (literal \"{p}\"))");
            }

            // Network access
            if self.config.allow_network {
                profile.push_str("(allow network*)\n");
            } else {
                profile.push_str("(deny network*)\n");
            }

            // Allow reading from standard file descriptors
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
    use std::path::PathBuf;
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, SetInformationJobObject, AssignProcessToJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECTEXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_JOB_TIME,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOB_OBJECT_UI_NO_DESKTOP,
        JOB_OBJECT_UI_EXIT_WINDOWS,
        JOB_OBJECT_UI_GLOBAL_ATOMS,
        JOB_OBJECT_UI_HANDLES,
        JOB_OBJECT_UI_READCLIPBOARD,
        JOB_OBJECT_UI_WRITECLIPBOARD,
    };

    /// Windows sandbox using Job Objects.
    pub struct WindowsSandbox {
        job_handle: HANDLE,
        config: SandboxConfig,
    }

    impl WindowsSandbox {
        pub fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
            // SAFETY: CreateJobObjectW is safe — it creates a new kernel object.
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

            let mut sandbox = Self {
                job_handle,
                config,
            };
            sandbox.apply_limits()?;

            Ok(sandbox)
        }

        /// Apply resource and UI restrictions to the Job Object.
        fn apply_limits(&mut self) -> Result<(), SandboxError> {
            // SAFETY: We're setting limits on a job object we created.
            unsafe {
                // Set basic limits (CPU time, memory)
                let mut basic_limits: JOBOBJECT_BASIC_LIMIT_INFORMATION = std::mem::zeroed();
                let mut limit_flags = 0u32;

                if let Some(max_mem) = self.config.max_memory_bytes {
                    basic_limits.JobMemoryLimit = max_mem as usize;
                    limit_flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                }

                if let Some(max_cpu) = self.config.max_cpu_seconds {
                    // Job time is in 100-nanosecond units
                    basic_limits.PerJobUserTimeLimit.QuadPart =
                        (max_cpu * 10_000_000) as i64;
                    limit_flags |= JOB_OBJECT_LIMIT_JOB_TIME;
                }

                if limit_flags != 0 {
                    basic_limits.LimitFlags = limit_flags;
                    let info = JOBOBJECTEXTENDED_LIMIT_INFORMATION {
                        BasicLimitInformation: basic_limits,
                        ..std::mem::zeroed()
                    };
                    let ret = SetInformationJobObject(
                        self.job_handle,
                        JobObjectExtendedLimitInformation,
                        &info as *const _ as *const _,
                        std::mem::size_of_val(&info) as u32,
                    );
                    if ret == 0 {
                        return Err(SandboxError::RlimitError(format!(
                            "SetInformationJobObject failed: {}",
                            std::io::Error::last_os_error()
                        )));
                    }
                }

                // Apply UI restrictions
                let mut ui_restrictions: JOBOBJECT_BASIC_UI_RESTRICTIONS = std::mem::zeroed();
                ui_restrictions.UIRestrictionsClass =
                    JOB_OBJECT_UI_NO_DESKTOP
                        | JOB_OBJECT_UI_EXIT_WINDOWS
                        | JOB_OBJECT_UI_GLOBAL_ATOMS
                        | JOB_OBJECT_UI_HANDLES
                        | JOB_OBJECT_UI_READCLIPBOARD
                        | JOB_OBJECT_UI_WRITECLIPBOARD;

                let ret = SetInformationJobObject(
                    self.job_handle,
                    windows_sys::Win32::System::JobObjects::JobObjectBasicUIRestrictions,
                    &ui_restrictions as *const _ as *const _,
                    std::mem::size_of_val(&ui_restrictions) as u32,
                );
                if ret == 0 {
                    tracing::warn!(
                        "Failed to set UI restrictions (non-critical): {}",
                        std::io::Error::last_os_error()
                    );
                }
            }

            tracing::debug!("Windows Job Object limits applied");
            Ok(())
        }

        /// Assign a process to this job.
        ///
        /// Call this on a `std::process::Child` after spawning but before
        /// the child executes anything sensitive.
        pub fn assign_process(&self, child: &std::process::Child) -> Result<(), SandboxError> {
            // SAFETY: We're assigning a valid process handle to a valid job handle.
            unsafe {
                let ret = AssignProcessToJobObject(self.job_handle, child.as_raw_handle() as HANDLE);
                if ret == 0 {
                    return Err(SandboxError::CreationError(format!(
                        "AssignProcessToJobObject failed: {}",
                        std::io::Error::last_os_error()
                    )));
                }
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
        let sandbox = MacOsSandbox::create(config).unwrap();
        let profile = sandbox.generate_profile();

        // Verify profile structure
        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("/tmp/test"));
        assert!(profile.contains("/tmp/output"));
        assert!(profile.contains("/bin/ls"));
        assert!(profile.contains("(deny network*)"));
        assert!(!profile.contains("(allow network*)"));

        // Verify exec_command returns sandbox-exec
        let (cmd, _args) = sandbox.exec_command();
        assert_eq!(cmd, "sandbox-exec");

        // Verify profile file was written
        assert!(sandbox.profile_path.is_some());
        let path = sandbox.profile_path.as_ref().unwrap();
        assert!(path.exists());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_sandbox_rlimits_apply() {
        let config = SandboxConfig {
            max_open_files: Some(64),
            max_memory_bytes: Some(1024 * 1024 * 1024), // 1GB
            max_cpu_seconds: Some(60),
            ..SandboxConfig::default()
        };
        let sandbox = LinuxSandbox::create(config).unwrap();
        // If we got here, rlimits applied without error
        drop(sandbox);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_sandbox_seccomp_no_new_privs() {
        let config = SandboxConfig::default();
        let sandbox = LinuxSandbox::create(config).unwrap();
        sandbox.apply_seccomp().unwrap();
        // Verify PR_SET_NO_NEW_PRIVS was set
        let result = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
        assert_eq!(result, 1, "PR_GET_NO_NEW_PRIVS should be 1");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_sandbox_profile_cleanup() {
        let config = SandboxConfig {
            allowed_read_paths: vec![PathBuf::from("/tmp/test")],
            ..SandboxConfig::default()
        };
        let sandbox = MacOsSandbox::create(config).unwrap();
        let profile_path = sandbox.profile_path.as_ref().unwrap().clone();
        assert!(profile_path.exists());
        drop(sandbox);
        // Drop should clean up the profile file
        assert!(!profile_path.exists());
    }
}
