//! Default sandbox policies per tool.
//!
//! When sandboxing is enabled in [`ExecutionConfig`], the [`RegistryToolDispatcher`]
//! injects a [`SandboxConfig`] into `ToolContext.extra` under the key `"sandbox_config"`.
//! Each tool can then read that config and apply platform-specific restrictions.
//!
//! ## Default policies
//!
//! | Tool       | Read paths | Write paths | Network | Exec paths |
//! |------------|------------|-------------|---------|------------|
//! | `Bash`     | `cwd`      | `cwd`       | `false` | system bins |
//! | `Edit`     | `cwd`      | `cwd`       | `false` | —          |
//! | `Write`    | `cwd`      | `cwd`       | `false` | —          |
//! | *others*   | `cwd`      | `cwd`       | `false` | —          |

use std::path::Path;

/// Build a `SandboxConfig` for the given tool name and working directory.
///
/// Returns `None` for tools that don't spawn subprocesses or access the
/// filesystem, so they run without sandbox overhead.
#[must_use]
pub fn default_sandbox_config(
    tool_name: &str,
    cwd: &Path,
) -> Option<coderex_core::sandbox::SandboxConfig> {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());

    match tool_name {
        "Bash" | "bash" | "PowerShell" | "powershell" => {
            Some(coderex_core::sandbox::SandboxConfig {
                allowed_read_paths: vec![canonical_cwd.clone()],
                allowed_write_paths: vec![canonical_cwd.clone()],
                allowed_exec_paths: system_exec_paths(),
                additional_working_dirs: vec![canonical_cwd],
                max_open_files: Some(256),
                max_memory_bytes: Some(512 * 1024 * 1024), // 512 MB
                max_cpu_seconds: Some(300),                // 5 minutes
                allow_network: false,
            })
        }
        "Edit" | "edit" | "Write" | "write" => Some(coderex_core::sandbox::SandboxConfig {
            allowed_read_paths: vec![canonical_cwd.clone()],
            allowed_write_paths: vec![canonical_cwd.clone()],
            allowed_exec_paths: vec![],
            additional_working_dirs: vec![canonical_cwd],
            max_open_files: Some(64),
            max_memory_bytes: Some(64 * 1024 * 1024), // 64 MB
            max_cpu_seconds: Some(60),
            allow_network: false,
        }),
        // Default policy for any other tool that touches files or spawns processes
        _ => Some(coderex_core::sandbox::SandboxConfig {
            allowed_read_paths: vec![canonical_cwd.clone()],
            allowed_write_paths: vec![canonical_cwd.clone()],
            allowed_exec_paths: vec![],
            additional_working_dirs: vec![canonical_cwd],
            max_open_files: Some(128),
            max_memory_bytes: Some(256 * 1024 * 1024),
            max_cpu_seconds: Some(120),
            allow_network: false,
        }),
    }
}

/// Common system executable paths allowed in sandboxed shells.
#[must_use]
fn system_exec_paths() -> Vec<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![
            std::path::PathBuf::from("/bin"),
            std::path::PathBuf::from("/usr/bin"),
            std::path::PathBuf::from("/usr/local/bin"),
        ]
    }
    #[cfg(target_os = "linux")]
    {
        vec![
            std::path::PathBuf::from("/bin"),
            std::path::PathBuf::from("/usr/bin"),
            std::path::PathBuf::from("/usr/local/bin"),
        ]
    }
    #[cfg(target_os = "windows")]
    {
        vec![]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        vec![]
    }
}

/// Check whether a resolved path is inside any of the allowed paths.
///
/// Used by `EditTool` and `WriteTool` to enforce sandbox boundaries
/// even when the platform sandbox doesn't restrict file ops at the
/// syscall level (e.g. macOS sandbox-exec allows broad reads).
#[must_use]
pub fn is_path_allowed(path: &std::path::Path, allowed: &[std::path::PathBuf]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    allowed
        .iter()
        .any(|allowed_path| canonical.starts_with(allowed_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::path::PathBuf;

    #[test]
    fn bash_policy_has_limits() {
        let config = default_sandbox_config("Bash", Path::new("/tmp")).unwrap();
        assert!(!config.allow_network);
        assert!(config.max_memory_bytes.is_some());
        assert!(config.max_cpu_seconds.is_some());
    }

    #[test]
    fn edit_policy_has_cwd_paths() {
        let config = default_sandbox_config("Edit", Path::new("/tmp")).unwrap();
        assert!(!config.allowed_read_paths.is_empty());
        assert!(!config.allow_network);
    }

    #[test]
    fn unknown_tool_gets_default_policy() {
        let config = default_sandbox_config("Unknown", Path::new("/tmp"));
        assert!(config.is_some());
    }

    #[test]
    fn path_allowed_when_inside_boundary() {
        let allowed = vec![PathBuf::from("/tmp")];
        assert!(is_path_allowed(Path::new("/tmp/foo.txt"), &allowed));
    }

    #[test]
    fn path_denied_when_outside_boundary() {
        let allowed = vec![PathBuf::from("/tmp")];
        assert!(!is_path_allowed(Path::new("/etc/passwd"), &allowed));
    }

    #[test]
    fn path_allowed_when_no_restrictions() {
        let allowed: Vec<PathBuf> = vec![];
        assert!(is_path_allowed(Path::new("/anywhere"), &allowed));
    }

    fn path_components() -> impl Strategy<Value = Vec<String>> {
        proptest::collection::vec("[a-z0-9_-]{1,8}", 1..4)
    }

    proptest! {
        #[test]
        fn every_canonical_descendant_of_an_allowed_root_is_allowed(
            components in path_components(),
        ) {
            let root = tempfile::tempdir().unwrap();
            let mut path = root.path().to_path_buf();
            for component in &components {
                path.push(component);
            }
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "x").unwrap();

            let allowed = vec![std::fs::canonicalize(root.path()).unwrap()];
            prop_assert!(is_path_allowed(&path, &allowed), "{}", path.display());
        }

        #[test]
        fn a_sibling_root_is_not_inside_the_allowed_root(
            right in path_components(),
        ) {
            let root = tempfile::tempdir().unwrap();
            let allowed_root = root.path().join("allowed");
            std::fs::create_dir(&allowed_root).unwrap();
            let allowed_root = std::fs::canonicalize(&allowed_root).unwrap();
            let mut outside = root.path().to_path_buf();
            for component in &right {
                outside.push(component);
            }
            std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
            std::fs::write(&outside, "x").unwrap();
            let outside = std::fs::canonicalize(&outside).unwrap();

            let allowed = vec![allowed_root.clone()];
            prop_assume!(!outside.starts_with(&allowed_root));
            prop_assert!(!is_path_allowed(&outside, &allowed), "{}", outside.display());
        }
    }
}
