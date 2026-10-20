//! Sandbox policy for stdio MCP servers.
//!
//! An MCP server is a program named by repository-writable configuration, so
//! it gets the same treatment as a shell command: it runs under the platform
//! sandbox when one exists. The policy is deliberately wider than a shell
//! command's, because an MCP server legitimately needs to read its own
//! installation and package caches, and many servers exist to talk to a remote
//! API (so the network is allowed by default).
//!
//! `CODEREX_MCP_SANDBOX=0` or a server's `"sandbox": {"enabled": false}`
//! opts out. On Windows a missing AppContainer runtime is a refusal, not a
//! silent downgrade: the server does not start.

use crate::config::McpSandboxConfig;
use coderex_core::sandbox::{SandboxConfig, resolve_program};
use std::path::{Path, PathBuf};

/// Set to `0`, `false` or `no` to run stdio servers without a sandbox.
pub const MCP_SANDBOX_ENV: &str = "CODEREX_MCP_SANDBOX";

/// Whether the environment switched server sandboxing off.
fn disabled_by_environment() -> bool {
    std::env::var(MCP_SANDBOX_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no"
        )
    })
}

/// Build the sandbox configuration for a stdio server, if it should be
/// confined.
///
/// Returns `None` when the server opted out. The caller decides what a
/// failure to *create* the sandbox means; this function only decides policy.
#[must_use]
pub fn stdio_sandbox_config(
    command: &str,
    sandbox: &McpSandboxConfig,
    cwd: &Path,
) -> Option<SandboxConfig> {
    if !sandbox.enabled || disabled_by_environment() {
        return None;
    }

    let workspace = cwd.to_path_buf();
    let workspace = canonical(&workspace);

    // Read: the workspace, the server's own installation (its interpreter and
    // the package tree it lives in), the user's config, and the package caches
    // that a `npx`/`uvx` launcher reads on every start.
    let mut read = vec![workspace.clone()];
    let mut exec = Vec::new();
    if let Some(program) = resolve_program(command, &workspace) {
        if let Some(parent) = program.parent() {
            // On Windows the sandbox's own plan already grants the system
            // roots and `PATH` directories — best-effort. Repeating them here
            // would make the grant required, and a directory like
            // `C:\Program Files\nodejs` cannot be rewritten by an ordinary
            // user, which would turn "cannot tighten this DACL" into "the
            // server does not start".
            if !platform_covers(parent) {
                exec.push(parent.to_path_buf());
                read.push(parent.to_path_buf());
            }
            // A toolchain usually lives at `<prefix>/bin`; the interpreter
            // needs its standard library under the prefix as well.
            if parent.file_name().is_some_and(|name| name == "bin") {
                if let Some(prefix) = parent.parent() {
                    if !platform_covers(prefix) {
                        read.push(prefix.to_path_buf());
                    }
                }
            }
        }
    }
    if let Some(home) = home_dir() {
        // Read paths are granted recursively. On Windows, making the whole
        // user profile reachable would walk it once per server start, so only
        // the package caches below are granted there; a server that needs
        // another profile location names it in `allowed_read_paths`.
        #[cfg(not(target_os = "windows"))]
        read.push(home.clone());
        for cache in [
            home.join(".npm"),
            home.join(".cache"),
            home.join(".local").join("share"),
            home.join("AppData").join("Roaming").join("npm"),
            home.join("AppData").join("Local").join("npm-cache"),
        ] {
            if cache.is_dir() {
                read.push(canonical(&cache));
            }
        }
    }

    let mut write = vec![workspace.clone()];
    read.extend(sandbox.allowed_read_paths.iter().cloned());
    write.extend(sandbox.allowed_write_paths.iter().cloned());
    exec.extend(sandbox.allowed_exec_paths.iter().cloned());

    Some(SandboxConfig {
        allowed_read_paths: read,
        allowed_write_paths: write,
        allowed_exec_paths: exec,
        additional_working_dirs: vec![workspace],
        max_open_files: None,
        max_memory_bytes: None,
        max_cpu_seconds: None,
        allow_network: sandbox.allow_network,
    })
}

/// Whether the platform sandbox covers `path` without an explicit grant.
///
/// On Windows that means a system root or a `PATH` directory: `acl_plan`
/// grants both best-effort. Those directories are frequently owned by the
/// system (TrustedInstaller), so a required grant would fail.
// Windows reads the environment; elsewhere it is a constant `false`, which is
// why clippy offers `const` for this build only.
#[allow(clippy::missing_const_for_fn)]
fn platform_covers(path: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        let roots = [
            "SystemRoot",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
        ]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from);
        let local_apps = std::env::var_os("LOCALAPPDATA")
            .map(|local| PathBuf::from(local).join("Microsoft").join("WindowsApps"));
        let roots = roots.chain(local_apps);
        if roots.into_iter().any(|root| path.starts_with(root)) {
            return true;
        }
        return std::env::var_os("PATH").is_some_and(|entries| {
            std::env::split_paths(&entries).any(|dir| path.starts_with(dir))
        });
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        false
    }
}

/// Canonicalize when possible, keeping the original path otherwise.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The user's home directory, if the platform names one.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    // `std::env::set_var` is unsafe on edition 2024; this module's only
    // environment mutation is serialized by the test that owns it.
    #![allow(unsafe_code)]

    use super::*;

    fn sandbox() -> McpSandboxConfig {
        McpSandboxConfig::default()
    }

    fn policy(cwd: &Path) -> Option<SandboxConfig> {
        stdio_sandbox_config("node", &sandbox(), cwd)
    }

    #[test]
    fn default_policy_confines_the_workspace_and_allows_the_network() {
        let cwd = std::env::current_dir().unwrap();
        let config = policy(&cwd).expect("enabled by default");
        assert!(config.allowed_read_paths.contains(&canonical(&cwd)));
        assert!(config.allowed_write_paths.contains(&canonical(&cwd)));
        assert_eq!(config.additional_working_dirs, vec![canonical(&cwd)]);
        assert!(
            config.allow_network,
            "servers reach their upstream API by default"
        );
    }

    #[test]
    fn a_server_can_opt_out() {
        let mut sandbox = sandbox();
        sandbox.enabled = false;
        assert!(stdio_sandbox_config("node", &sandbox, Path::new(".")).is_none());
    }

    #[test]
    fn the_environment_can_switch_the_sandbox_off() {
        // The environment is process-wide; this test owns the variable for the
        // duration and restores it.
        let previous = std::env::var_os(MCP_SANDBOX_ENV);
        // SAFETY: this test is the only one that touches this variable.
        unsafe { std::env::set_var(MCP_SANDBOX_ENV, "0") };
        let disabled = policy(Path::new("."));
        match previous {
            Some(value) => unsafe { std::env::set_var(MCP_SANDBOX_ENV, value) },
            None => unsafe { std::env::remove_var(MCP_SANDBOX_ENV) },
        }
        assert!(disabled.is_none());
    }

    #[test]
    fn extra_paths_extend_the_policy_and_must_be_absolute() {
        let mut sandbox = sandbox();
        sandbox.allowed_read_paths = vec![PathBuf::from("/opt/mcp-data")];
        let config = stdio_sandbox_config("node", &sandbox, Path::new("/work")).unwrap();
        assert!(
            config
                .allowed_read_paths
                .contains(&PathBuf::from("/opt/mcp-data"))
        );

        sandbox.allowed_write_paths = vec![PathBuf::from("relative/path")];
        assert!(sandbox.validate().is_err());
    }
}
