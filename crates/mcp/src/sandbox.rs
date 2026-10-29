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
//! opts out.
//!
//! # Failure policy: fail closed
//!
//! A server that was meant to be confined does not run unconfined. When a
//! policy is in force and this platform cannot provide confinement — Windows
//! has no AppContainer runtime, Linux has neither bubblewrap nor Landlock,
//! macOS has no `sandbox-exec` — [`create_stdio_sandbox`] returns an error and
//! the server is refused. This is the *gating* branch of the hook engine's
//! honest-degradation policy (`crates/hooks/src/sandbox.rs`): an MCP server is
//! arbitrary code named by repository configuration, and the agent will call
//! the tools it returns, so it is held to the same standard as a gating hook
//! rather than the non-gating "limits-only" fallback. The escape hatches are
//! the same ones that turn the policy off in the first place, and the error
//! names them.

use crate::config::McpSandboxConfig;
use coderex_core::sandbox::{SandboxConfig, SandboxHandle, resolve_program};
use std::path::{Path, PathBuf};

/// Set to `0`, `false` or `no` to run stdio servers without a sandbox.
pub const MCP_SANDBOX_ENV: &str = "CODEREX_MCP_SANDBOX";

/// Whether an environment value is an explicit negative.
///
/// Consistent with `CODEREX_BWRAP`: anything but `0`, `false` or `no` leaves
/// the sandbox on, because a sandbox that switches itself off over an
/// unexpected value is worse than one that says what it wants.
fn env_value_disables(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no"
    )
}

/// Whether the environment switched server sandboxing off.
fn disabled_by_environment() -> bool {
    std::env::var(MCP_SANDBOX_ENV).is_ok_and(|value| env_value_disables(&value))
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
    stdio_sandbox_config_with(command, sandbox, cwd, disabled_by_environment())
}

/// The injectable core of [`stdio_sandbox_config`], so the environment opt-out
/// can be tested without mutating the process-global variable.
fn stdio_sandbox_config_with(
    command: &str,
    sandbox: &McpSandboxConfig,
    cwd: &Path,
    disabled_by_env: bool,
) -> Option<SandboxConfig> {
    if !sandbox.enabled || disabled_by_env {
        return None;
    }

    let workspace = cwd.to_path_buf();
    let workspace = canonical(&workspace);

    // Read: the workspace, the server's own installation (its interpreter and
    // the package tree it lives in), the user's config, and the package caches
    // that a `npx`/`uvx` launcher reads on every start.
    let mut read = vec![workspace.clone()];
    let mut exec = Vec::new();
    if let Some(program) = resolve_program(command, &workspace)
        && let Some(parent) = program.parent()
    {
        // On Windows the sandbox's own plan already grants the system
        // roots and `PATH` directories — best-effort. Repeating them here
        // would make the grant required, and a directory like
        // `C:\Program Files\nodejs` cannot be rewritten by an ordinary
        // user, which would turn "cannot tighten this DACL" into "the
        // server does not start".
        if !platform_covers(parent) {
            // Canonicalized because the wrapper matches on the resolved
            // path: on macOS `/var` is a symlink to `/private/var`, so a
            // raw path grants a `subpath` rule the kernel never sees.
            exec.push(canonical(parent));
            read.push(canonical(parent));
        }
        // A toolchain usually lives at `<prefix>/bin`; the interpreter
        // needs its standard library under the prefix as well.
        if parent.file_name().is_some_and(|name| name == "bin")
            && let Some(prefix) = parent.parent()
            && !platform_covers(prefix)
        {
            read.push(canonical(prefix));
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
    // A launcher resolves its interpreter through `PATH` or a shebang, so the
    // directories on `PATH` — and the prefix a `bin` directory belongs to, for
    // the interpreter's standard library — must be readable and executable.
    // They are never writable. Windows is excluded: its ACL plan already grants
    // the system roots and `PATH` directories best-effort, and repeating them
    // here would make the grant required.
    #[cfg(not(target_os = "windows"))]
    for dir in path_exec_dirs() {
        if dir
            .file_name()
            .is_some_and(|name| name == "bin" || name == "sbin")
            && let Some(prefix) = dir.parent()
        {
            push_unique(&mut read, prefix.to_path_buf());
        }
        push_unique(&mut read, dir.clone());
        push_unique(&mut exec, dir);
    }

    let mut write = vec![workspace.clone()];
    // The temp directory is writable as well: an interpreter and the launcher
    // above it use it for scratch files, and denying it breaks servers that
    // would otherwise run. Windows supplies the container a private temp
    // directory, so it is excluded there — granting the shared tree would lower
    // its integrity label and let every Low-integrity process write there.
    #[cfg(not(target_os = "windows"))]
    {
        let temp = canonical(&std::env::temp_dir());
        push_unique(&mut read, temp.clone());
        push_unique(&mut write, temp);
    }
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

/// Create the sandbox for a stdio server, failing closed.
///
/// # Policy
///
/// An MCP server is confined like a *gating* hook rather than a non-gating one:
/// when a policy is in force and this platform cannot provide filesystem
/// confinement, the server is refused instead of silently running unconfined.
/// `CODEREX_MCP_SANDBOX=0` and a server's `"sandbox": {"enabled": false}` are
/// the explicit escape hatches; both make [`stdio_sandbox_config`] return
/// `None`, so this function is never reached for an opted-out server.
///
/// # Errors
///
/// Returns a message naming the escape hatch when confinement is unavailable,
/// or the underlying error when the sandbox handle cannot be created.
pub fn create_stdio_sandbox(config: &SandboxConfig) -> Result<SandboxHandle, String> {
    create_with(config, confinement_availability())
}

/// The injectable core of [`create_stdio_sandbox`], so the failure policy can
/// be tested without depending on what this host happens to support.
fn create_with(
    config: &SandboxConfig,
    availability: Result<(), String>,
) -> Result<SandboxHandle, String> {
    if let Err(reason) = availability {
        return Err(format!(
            "{reason}; refusing to run an MCP server without filesystem confinement \
             (set {MCP_SANDBOX_ENV}=0 or the server's sandbox.enabled=false to opt out)"
        ));
    }
    SandboxHandle::create(config.clone()).map_err(|error| error.to_string())
}

/// Whether this platform can actually apply filesystem confinement right now.
///
/// Mirrors the hook engine's probe (`crates/hooks/src/sandbox.rs`): the answer
/// is a runtime question on Linux (is bubblewrap or Landlock in force?) and on
/// macOS the wrapper program has to exist. Windows always reports available and
/// lets [`SandboxHandle::create`] fail closed when the AppContainer runtime is
/// missing, which keeps that decision in one place.
// Windows has no wrapper to look for, so this cannot fail there; the
// signature is shared with the platforms that check for one.
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
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
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err("this platform has no filesystem isolation".to_string())
    }
}

/// The absolute directories on `PATH`, resolved and de-duplicated.
///
/// An MCP server is normally a launcher — `npx`, `uvx`, `python`, `node` — that
/// resolves its interpreter through `PATH` or a shebang, so those directories
/// must be executable (and readable) or the sandbox denies the very `exec` that
/// starts the server. A relative entry, including `.`, is skipped: it means
/// "the current directory", and granting execute over the workspace just
/// because a user's `PATH` contains `.` would hand the server the thing the
/// sandbox exists to bound.
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
        push_unique(&mut dirs, canonical(&dir));
    }
    dirs
}

#[cfg(not(target_os = "windows"))]
fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
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
        std::env::var_os("PATH")
            .is_some_and(|entries| std::env::split_paths(&entries).any(|dir| path.starts_with(dir)))
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
    use super::*;

    fn sandbox() -> McpSandboxConfig {
        McpSandboxConfig::default()
    }

    /// The policy decision with the environment opt-out forced on or off, so
    /// the process-global `CODEREX_MCP_SANDBOX` never has to be touched.
    fn policy_with_env(disabled_by_env: bool) -> Option<SandboxConfig> {
        stdio_sandbox_config_with("node", &sandbox(), Path::new("."), disabled_by_env)
    }

    #[test]
    fn default_policy_confines_the_workspace_and_allows_the_network() {
        let cwd = std::env::current_dir().unwrap();
        let config =
            stdio_sandbox_config_with("node", &sandbox(), &cwd, false).expect("enabled by default");
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
        assert!(stdio_sandbox_config_with("node", &sandbox, Path::new("."), false).is_none());
    }

    /// `CODEREX_MCP_SANDBOX=0` is the global escape hatch. The decision is
    /// exercised through the injectable core rather than by mutating the
    /// process environment, which would race the other tests.
    #[test]
    fn the_environment_can_switch_the_sandbox_off() {
        assert!(policy_with_env(true).is_none());
        assert!(
            policy_with_env(false).is_some(),
            "without the opt-out the policy must be present"
        );
    }

    /// Only an explicit negative counts; an unexpected value leaves the
    /// sandbox on.
    #[test]
    fn only_an_explicit_negative_disables_the_sandbox() {
        for off in ["0", "false", "no", " FALSE "] {
            assert!(env_value_disables(off), "{off:?} should opt out");
        }
        for on in ["1", "true", "yes", "", "off"] {
            assert!(!env_value_disables(on), "{on:?} must leave the sandbox on");
        }
    }

    #[test]
    fn extra_paths_extend_the_policy_and_must_be_absolute() {
        let mut sandbox = sandbox();
        sandbox.allowed_read_paths = vec![PathBuf::from("/opt/mcp-data")];
        let config =
            stdio_sandbox_config_with("node", &sandbox, Path::new("/work"), false).unwrap();
        assert!(
            config
                .allowed_read_paths
                .contains(&PathBuf::from("/opt/mcp-data"))
        );

        sandbox.allowed_write_paths = vec![PathBuf::from("relative/path")];
        assert!(sandbox.validate().is_err());
    }

    /// An MCP server is a launcher that resolves its interpreter through
    /// `PATH` or a shebang, so the directories on `PATH` — and the prefix of a
    /// `bin` directory, for the interpreter's standard library — must be
    /// executable and readable, or the sandbox denies the very `exec` that
    /// starts the server. Windows is excluded: its ACL plan covers these.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn the_path_directories_are_executable_and_readable() {
        let Some(dir) = path_exec_dirs().into_iter().next() else {
            eprintln!("skipping: PATH names no absolute directory");
            return;
        };
        let config =
            stdio_sandbox_config_with("node", &sandbox(), Path::new("/work"), false).unwrap();
        assert!(
            config.allowed_exec_paths.contains(&dir),
            "a PATH directory must be executable: {}",
            dir.display()
        );
        assert!(
            config.allowed_read_paths.contains(&dir),
            "a PATH directory must be readable: {}",
            dir.display()
        );
        // A relative entry means "the current directory" and is skipped.
        assert!(
            !config
                .allowed_exec_paths
                .contains(&PathBuf::from("relative/bin"))
        );
    }

    /// When the platform cannot confine, the server is refused rather than run
    /// unconfined, and the error names the escape hatch.
    #[test]
    fn an_unavailable_platform_refuses_instead_of_running_unconfined() {
        let Err(error) = create_with(
            &SandboxConfig::default(),
            Err("bubblewrap (bwrap) is not installed".to_string()),
        ) else {
            panic!("an unavailable sandbox must be a refusal, not a fallback");
        };
        assert!(
            error.contains(MCP_SANDBOX_ENV),
            "the refusal must name the escape hatch: {error}"
        );
        assert!(
            error.contains("refusing to run an MCP server"),
            "the refusal must say what it refused: {error}"
        );
    }

    /// Where confinement *is* available, a handle is produced on every platform
    /// this test can run on.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn an_available_platform_creates_a_handle() {
        if confinement_availability().is_err() {
            eprintln!("skipping: this host has no filesystem confinement available");
            return;
        }
        let config =
            stdio_sandbox_config_with("node", &sandbox(), &std::env::current_dir().unwrap(), false)
                .unwrap();
        assert!(create_stdio_sandbox(&config).is_ok());
    }
}
