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

    /// Get the program to use as a sandbox wrapper (macOS only).
    ///
    /// On macOS, returns `("sandbox-exec", ["-f", profile_path])`.
    /// On other platforms, returns `None`.
    #[cfg(target_os = "macos")]
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

    fn prepare(&self, _cmd: &mut std::process::Command) -> Result<(), SandboxError> {
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

    #[cfg(target_os = "macos")]
    fn sandboxed_program(&self) -> Option<(&str, Vec<String>)> {
        match self {
            Self::MacOs(s) => Some(s.exec_command()),
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
    use std::process::Command;

    /// Linux sandbox using seccomp + rlimit.
    pub struct LinuxSandbox {
        config: SandboxConfig,
    }

    impl LinuxSandbox {
        pub fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
            Ok(Self { config })
        }

        pub fn prepare(&self, cmd: &mut Command) -> Result<(), SandboxError> {
            let config = self.config.clone();
            let apply = move || -> Result<(), std::io::Error> {
                Self::apply_rlimits(&config)?;
                Self::apply_seccomp(config.allow_network)?;
                Ok(())
            };
            // SAFETY: pre_exec runs in the child process before exec().
            // The closure captures only `config` (Clone) and references no
            // parent-process state that would be invalid after fork.
            unsafe { cmd.pre_exec(apply) };
            Ok(())
        }

        pub fn post_spawn(&self, _child: &std::process::Child) -> Result<(), SandboxError> {
            Ok(())
        }

        // ─── rlimits ─────────────────────────────────────────────────────────

        fn apply_rlimits(config: &SandboxConfig) -> Result<(), std::io::Error> {
            use libc::{RLIMIT_AS, RLIMIT_CPU, RLIMIT_NOFILE, prlimit, rlimit64};

            if let Some(max_files) = config.max_open_files {
                let rl = rlimit64 {
                    rlim_cur: max_files,
                    rlim_max: max_files,
                };
                let ret = unsafe { prlimit(0, RLIMIT_NOFILE, &rl, std::ptr::null_mut()) };
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            if let Some(max_mem) = config.max_memory_bytes {
                let rl = rlimit64 {
                    rlim_cur: max_mem,
                    rlim_max: max_mem,
                };
                let ret = unsafe { prlimit(0, RLIMIT_AS, &rl, std::ptr::null_mut()) };
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            if let Some(max_cpu) = config.max_cpu_seconds {
                let rl = rlimit64 {
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

            // load arch @ offset 0
            filter.push(sock_filter {
                code: BPF_LD + BPF_W + BPF_ABS,
                jt: 0,
                jf: 0,
                k: 0,
            });
            // if arch == AUDIT_ARCH, continue; else kill
            filter.push(sock_filter {
                code: BPF_JMP + BPF_JEQ + BPF_K,
                jt: 0,
                jf: 1,
                k: super::seccomp_arch::AUDIT_ARCH,
            });
            filter.push(sock_filter {
                code: BPF_RET + BPF_K,
                jt: 0,
                jf: 0,
                k: SECCOMP_RET_KILL,
            });

            // load syscall @ offset 4
            filter.push(sock_filter {
                code: BPF_LD + BPF_W + BPF_ABS,
                jt: 0,
                jf: 0,
                k: 4,
            });

            // Network syscalls are absent from the base list, and the default
            // action is to kill — so without this the sandbox would block
            // network even when the config allows it.
            let mut allowed: Vec<i64> = ALLOWED.to_vec();
            if allow_network {
                allowed.extend_from_slice(super::seccomp_arch::NETWORK);
            }

            let mut filter: Vec<sock_filter> = Vec::with_capacity(4 + allowed.len() * 2 + 1);

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

            // default deny
            filter.push(sock_filter {
                code: BPF_RET + BPF_K,
                jt: 0,
                jf: 0,
                k: SECCOMP_RET_KILL,
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

            // Broad read access — the sandbox restricts writes, network,
            // and arbitrary execution, not reading.
            profile.push_str("(allow file-read*)\n");
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

            for path in &self.config.allowed_read_paths {
                let p = path.to_string_lossy();
                let _ = writeln!(profile, "(allow file-read* (subpath \"{p}\"))");
            }

            for path in &self.config.allowed_write_paths {
                let p = path.to_string_lossy();
                let _ = writeln!(profile, "(allow file-write* (subpath \"{p}\"))");
            }

            for dir in &self.config.additional_working_dirs {
                let d = dir.to_string_lossy();
                let _ = writeln!(profile, "(allow file-read* file-write* (subpath \"{d}\"))");
            }

            for path in &self.config.allowed_exec_paths {
                let p = path.to_string_lossy();
                let _ = writeln!(profile, "(allow process-exec (literal \"{p}\"))");
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
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_UI_EXIT_WINDOWS,
        JOB_OBJECT_UI_GLOBAL_ATOMS, JOB_OBJECT_UI_HANDLES, JOB_OBJECT_UI_NO_DESKTOP,
        JOB_OBJECT_UI_READCLIPBOARD, JOB_OBJECT_UI_WRITECLIPBOARD,
        JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
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

                if limit_flags != 0 {
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
            unsafe {
                let ret =
                    AssignProcessToJobObject(self.job_handle, child.as_raw_handle() as HANDLE);
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
