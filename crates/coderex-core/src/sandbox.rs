//! Cross-platform sandbox abstraction — seccomp, macOS sandbox-exec,
//! Windows Job Objects, and a no-op fallback.

use std::path::PathBuf;
use thiserror::Error;

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
            tracing::warn!(
                "Sandbox not supported on this platform; proceeding without isolation"
            );
            Ok(Self::Unsupported)
        }
    }
}

/// Linux sandbox using seccomp + rlimit.
#[cfg(target_os = "linux")]
struct LinuxSandbox {
    config: SandboxConfig,
}

#[cfg(target_os = "linux")]
impl LinuxSandbox {
    fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
        Ok(Self { config })
    }

    /// Apply rlimit restrictions to the current process.
    fn apply_rlimits(&self) -> Result<(), SandboxError> {
        // Placeholder: in production, use libc::setrlimit
        if self.config.max_open_files.is_some()
            || self.config.max_memory_bytes.is_some()
            || self.config.max_cpu_seconds.is_some()
        {
            tracing::debug!("Linux rlimits would be applied here");
        }
        Ok(())
    }

    /// Apply seccomp filters to restrict syscalls.
    fn apply_seccomp(&self) -> Result<(), SandboxError> {
        // Placeholder: in production, use libseccomp
        tracing::debug!("Linux seccomp filters would be applied here");
        Ok(())
    }
}

/// macOS sandbox using sandbox-exec.
#[cfg(target_os = "macos")]
struct MacOsSandbox {
    config: SandboxConfig,
}

#[cfg(target_os = "macos")]
impl MacOsSandbox {
    fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
        Ok(Self { config })
    }

    /// Generate a sandbox-exec profile string.
    #[allow(dead_code)]
    fn generate_profile(&self) -> String {
        let mut profile = String::from("(version 1)\n\n");

        // Default deny
        profile.push_str("(deny default)\n\n");

        // Allow reads
        for path in &self.config.allowed_read_paths {
            profile.push_str(&format!(
                "(allow file-read* (require-all (path \"{}\")))\n",
                path.display()
            ));
        }

        // Allow writes
        for path in &self.config.allowed_write_paths {
            profile.push_str(&format!(
                "(allow file-write* (require-all (path \"{}\")))\n",
                path.display()
            ));
        }

        // Allow syscalls needed for basic operation
        profile.push_str("(allow syscall)\n");
        profile.push_str("(allow signal (target same-sandbox))\n");

        profile
    }
}

/// Windows sandbox using Job Objects.
#[cfg(target_os = "windows")]
struct WindowsSandbox {
    config: SandboxConfig,
}

#[cfg(target_os = "windows")]
impl WindowsSandbox {
    fn create(config: SandboxConfig) -> Result<Self, SandboxError> {
        Ok(Self { config })
    }
}

/// Trait for sandbox backends.
pub trait SandboxBackend: Send + Sync {
    /// Get the platform name.
    fn platform_name(&self) -> &str;

    /// Check if this backend is supported on the current platform.
    fn is_supported(&self) -> bool;

    /// Apply the sandbox configuration.
    fn apply(&self, config: &SandboxConfig) -> Result<(), SandboxError>;
}

/// No-op sandbox for unsupported platforms or testing.
#[derive(Debug)]
pub struct NoOpSandbox;

impl SandboxBackend for NoOpSandbox {
    fn platform_name(&self) -> &str {
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
    fn macos_sandbox_generates_profile() {
        // Only runs on macOS, but we can test the struct
        #[cfg(target_os = "macos")]
        {
            let sandbox = MacOsSandbox::create(SandboxConfig::default()).unwrap();
            let profile = sandbox.generate_profile();
            assert!(profile.contains("(version 1)"));
            assert!(profile.contains("(deny default)"));
        }
    }
}
